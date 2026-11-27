//! Browser-based VERTEXIA login shared by the native client and Studio.
//!
//! The browser approves the login on the platform, which hands back a one-time code (through a
//! loopback callback, the URI scheme, or polling). The code is exchanged for a short-lived opaque
//! access token that is stored in the OS user config directory with 0600 permissions.

use crate::common::net::api::{ApiError, ExchangedLogin, LoginAudience, PlatformClient, TaskPoll};
use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredCredentials {
    pub username: String,
    pub uid: i32,
    /// Opaque bearer token issued by `/api/v1/studio/exchange`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    /// Unix seconds; `None` when the platform did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Long-lived legacy key from platforms that predate access tokens (Studio only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ukey: String,
}

impl std::fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCredentials")
            .field("username", &self.username)
            .field("uid", &self.uid)
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("ukey", &if self.ukey.is_empty() { "" } else { "<redacted>" })
            .finish()
    }
}

impl StoredCredentials {
    pub fn from_login(login: ExchangedLogin) -> Self {
        Self {
            username: login.username,
            uid: login.uid,
            access_token: login.access_token,
            expires_at: login.expires_at,
            ukey: login.legacy_ukey,
        }
    }

    pub fn bearer(&self) -> Option<&str> {
        Some(self.access_token.as_str()).filter(|t| !t.is_empty())
    }

    pub fn is_expired_at(&self, now_unix: i64) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now_unix)
    }

    pub fn is_usable(&self) -> bool {
        (!self.access_token.is_empty() && !self.is_expired_at(unix_now()))
            || !self.ukey.is_empty()
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn website_base() -> String {
    crate::common::net::api::api_base()
}

pub fn generate_state() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Per-user configuration directory for VERTEXIA (never the executable directory).
pub fn user_config_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    #[cfg(target_os = "windows")]
    let base = non_empty("APPDATA").map(|p| p.join("VERTEXIA"));
    #[cfg(target_os = "macos")]
    let base = non_empty("HOME").map(|h| h.join("Library/Application Support/VERTEXIA"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = non_empty("XDG_CONFIG_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| non_empty("HOME").map(|h| h.join(".config")))
        .map(|p| p.join("vertexia"));
    base
}

fn credentials_file_name(audience: LoginAudience) -> &'static str {
    match audience {
        LoginAudience::Client => "client_auth.json",
        LoginAudience::Studio => "studio_auth.json",
    }
}

pub fn credentials_path(audience: LoginAudience) -> Option<PathBuf> {
    user_config_dir().map(|d| d.join(credentials_file_name(audience)))
}

/// Pre-token Studio builds wrote the ukey next to the executable and in other shared places.
fn legacy_studio_auth_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("vertexia_studio_auth.json"));
        }
    }
    if let Ok(dir) = std::env::current_dir() {
        candidates.push(dir.join("vertexia_studio_auth.json"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        candidates.push(PathBuf::from(appdata).join("VERTEXIA").join("studio_auth.json"));
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home.clone()).join(".vertexia").join("studio_auth.json"));
        candidates.push(PathBuf::from(home).join(".config").join("vertexia").join("studio_auth.json"));
    }
    candidates
}

/// Writes `data` so only the current user can read it: the directory is created 0700 and the
/// file is written to a 0600 temp file, synced and renamed into place.
pub fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("credentials path has no parent"))?;
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
    }
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("auth"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    restrict_permissions(path);
    Ok(())
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o077 != 0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn load_credentials_from(path: &Path) -> Option<StoredCredentials> {
    let data = std::fs::read_to_string(path).ok()?;
    restrict_permissions(path);
    serde_json::from_str::<StoredCredentials>(&data)
        .ok()
        .filter(StoredCredentials::is_usable)
}

pub fn save_credentials_to(path: &Path, creds: &StoredCredentials) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(creds).map_err(|e| e.to_string())?;
    write_private_file(path, &data).map_err(|e| format!("failed to store credentials: {e}"))
}

pub fn load_credentials(audience: LoginAudience) -> Option<StoredCredentials> {
    let path = credentials_path(audience)?;
    if let Some(creds) = load_credentials_from(&path) {
        return Some(creds);
    }
    if audience != LoginAudience::Studio {
        return None;
    }
    for legacy in legacy_studio_auth_candidates() {
        if legacy == path || !legacy.exists() {
            continue;
        }
        let creds = std::fs::read_to_string(&legacy)
            .ok()
            .and_then(|d| serde_json::from_str::<StoredCredentials>(&d).ok())
            .filter(StoredCredentials::is_usable);
        if let Some(creds) = creds {
            if save_credentials_to(&path, &creds).is_ok() {
                let _ = std::fs::remove_file(&legacy);
                info!("Moved Studio credentials to the user config directory");
            }
            return Some(creds);
        }
    }
    None
}

pub fn save_credentials(audience: LoginAudience, creds: &StoredCredentials) -> Result<(), String> {
    let path = credentials_path(audience)
        .ok_or_else(|| "no user config directory available".to_string())?;
    save_credentials_to(&path, creds)
}

/// Deletes the stored token without contacting the platform (used after a 401).
pub fn forget_credentials(audience: LoginAudience) {
    if let Some(path) = credentials_path(audience) {
        let _ = std::fs::remove_file(path);
    }
    if audience == LoginAudience::Studio {
        for legacy in legacy_studio_auth_candidates() {
            let _ = std::fs::remove_file(legacy);
        }
    }
}

/// Logs out: deletes the local token immediately and revokes it server-side in the background.
pub fn logout(audience: LoginAudience, creds: Option<&StoredCredentials>) {
    forget_credentials(audience);
    if let Some(token) = creds.and_then(|c| c.bearer()).map(str::to_string) {
        std::thread::spawn(move || {
            match PlatformClient::new().and_then(|client| client.logout(&token)) {
                Ok(()) => info!("Access token revoked"),
                Err(e) => warn!("Failed to revoke access token: {e}"),
            }
        });
    }
}

pub fn load_studio_credentials() -> Option<StoredCredentials> {
    load_credentials(LoginAudience::Studio)
}

pub fn save_studio_credentials(creds: &StoredCredentials) -> Result<(), String> {
    save_credentials(LoginAudience::Studio, creds)
}

/// Logs the user out of the flow's audience and clears any in-progress login.
pub fn log_out(store: &mut StudioAuthStore, flow: &mut StudioAuthFlow) {
    logout(flow.audience, store.credentials.take().as_ref());
    flow.reset();
}

pub fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        if std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .is_ok()
        {
            return;
        }
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &format!("\"{url}\"")])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
    }
}

pub fn parse_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut iter = pair.splitn(2, '=');
        let k = iter.next()?;
        let v = iter.next().unwrap_or("");
        if k == key {
            return Some(url_decode(v));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            let hex = format!("{h1}{h2}");
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                out.push(byte as char);
            } else {
                out.push('%');
                out.push(h1);
                out.push(h2);
            }
        } else if c == '+' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

pub fn try_extract_code_from_args() -> Option<(String, String)> {
    for arg in std::env::args().skip(1) {
        let lower = arg.to_lowercase();
        if lower.starts_with("vertexia-studio://") || lower.starts_with("vertexia://") {
            if let Some(q) = arg.split('?').nth(1) {
                let code = parse_query_param(q, "code");
                let state = parse_query_param(q, "state").unwrap_or_default();
                if let Some(c) = code {
                    if !c.is_empty() {
                        return Some((c, state));
                    }
                }
            }
        }
        if arg.contains("code=") && (arg.starts_with("http://") || arg.starts_with("vertexia")) {
            if let Some(q) = arg.split('?').nth(1) {
                if let Some(c) = parse_query_param(q, "code") {
                    let state = parse_query_param(q, "state").unwrap_or_default();
                    if !c.is_empty() {
                        return Some((c, state));
                    }
                }
            }
        }
    }
    None
}

pub fn exchange_code_for(code: &str, audience: LoginAudience) -> Result<StoredCredentials, String> {
    let client = PlatformClient::new().map_err(|e| e.to_string())?;
    let login = client
        .exchange_code(code, audience)
        .map_err(|e| format!("exchange failed: {e}"))?;
    if audience == LoginAudience::Client && login.access_token.is_empty() {
        return Err("The platform did not issue an access token; update VERTEXIA.".to_string());
    }
    Ok(StoredCredentials::from_login(login))
}

pub fn exchange_code(code: &str) -> Result<StoredCredentials, String> {
    exchange_code_for(code, LoginAudience::Studio)
}

#[derive(Resource)]
pub struct StudioAuthFlow {
    pub state: Option<String>,
    pub redirect_uri: Option<String>,
    pub receiver: Option<Receiver<Result<StoredCredentials, String>>>,
    pub error: Option<String>,
    pub waiting_since: Option<Instant>,
    pub is_polling: bool,
    pub audience: LoginAudience,
    /// Background `/me` check of a stored token; a 401 clears it and re-prompts.
    pub validation: Option<crate::common::net::api::ApiTask<Result<(), ApiError>>>,
}

impl Default for StudioAuthFlow {
    fn default() -> Self {
        Self::for_audience(LoginAudience::Studio)
    }
}

impl StudioAuthFlow {
    pub fn for_audience(audience: LoginAudience) -> Self {
        Self {
            state: None,
            redirect_uri: None,
            receiver: None,
            error: None,
            waiting_since: None,
            is_polling: false,
            audience,
            validation: None,
        }
    }

    /// Clears an in-progress login without touching stored credentials.
    pub fn reset(&mut self) {
        let audience = self.audience;
        *self = Self::for_audience(audience);
    }

    /// Exchanges a code delivered through the URI scheme on a worker thread.
    pub fn exchange_in_background(&mut self, code: String) {
        let (tx, rx) = unbounded();
        let audience = self.audience;
        std::thread::spawn(move || {
            let _ = tx.send(exchange_code_for(&code, audience));
        });
        self.receiver = Some(rx);
        self.error = None;
        self.waiting_since = Some(Instant::now());
        self.is_polling = true;
    }

    /// Verifies a stored access token with `/me` without blocking the frame.
    pub fn validate_in_background(&mut self, creds: &StoredCredentials) {
        let Some(token) = creds.bearer().map(str::to_string) else {
            return;
        };
        self.validation = Some(crate::common::net::api::ApiTask::spawn(move || {
            PlatformClient::new()?.me(&token).map(|_| ())
        }));
    }
}

#[derive(Resource, Default, Debug, Clone)]
pub struct StudioAuthStore {
    pub credentials: Option<StoredCredentials>,
}

impl StudioAuthStore {
    pub fn is_logged_in(&self) -> bool {
        self.credentials.as_ref().is_some_and(StoredCredentials::is_usable)
    }
}


pub const LOGIN_REQUIRED_TOOLTIP: &str =
    "This element cannot be inserted or updated because you are not logged in into VERTEXIA."; //String!

pub fn init_studio_auth(mut store: ResMut<StudioAuthStore>, mut flow: ResMut<StudioAuthFlow>) {
    *flow = StudioAuthFlow::default();
    let creds = load_studio_credentials();
    if let Some(creds) = &creds {
        flow.validate_in_background(creds);
    }
    store.credentials = creds;
    if let Some((code, _state)) = try_extract_code_from_args() {
        flow.exchange_in_background(code);
    }
    #[cfg(target_os = "windows")]
    {
        let _ = try_register_uri_scheme();
    }
}

#[cfg(target_os = "windows")]
fn try_register_uri_scheme() -> Result<(), String> {
    use std::process::Command;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_str = exe.display().to_string();
    let ps_script = format!(
        "$p='HKCU:\\Software\\Classes\\vertexia-studio'; if(!(Test-Path $p)){{New-Item -Path $p -Force | Out-Null}}; Set-ItemProperty -Path $p -Name '(Default)' -Value 'URL:VERTEXIA Studio'; Set-ItemProperty -Path $p -Name 'URL Protocol' -Value ''; $c=\"$p\\shell\\open\\command\"; if(!(Test-Path $c)){{New-Item -Path $c -Force | Out-Null}}; Set-ItemProperty -Path $c -Name '(Default)' -Value '\"{exe_str}\" \"%1\"'; $p2='HKCU:\\Software\\Classes\\vertexia'; if(!(Test-Path $p2)){{New-Item -Path $p2 -Force | Out-Null}}; Set-ItemProperty -Path $p2 -Name '(Default)' -Value 'URL:VERTEXIA'; Set-ItemProperty -Path $p2 -Name 'URL Protocol' -Value ''; $c2=\"$p2\\shell\\open\\command\"; if(!(Test-Path $c2)){{New-Item -Path $c2 -Force | Out-Null}}; Set-ItemProperty -Path $c2 -Name '(Default)' -Value '\"{exe_str}\" \"%1\"'",
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_script])
        .output();
    Ok(())
}

pub fn start_studio_auth_flow(flow: &mut StudioAuthFlow) -> Result<(), String> {
    let audience = flow.audience;
    start_auth_flow(flow, audience)
}

/// Opens the platform login page in the browser and waits for the code on a worker thread.
pub fn start_auth_flow(flow: &mut StudioAuthFlow, audience: LoginAudience) -> Result<(), String> {
    flow.audience = audience;
    let state = generate_state();
    let (tx, rx): (Sender<Result<StoredCredentials, String>>, Receiver<Result<StoredCredentials, String>>) = unbounded();
    let redirect_uri;
    let expected_state = state.clone();
    let base = website_base();
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            let port = listener.local_addr().map_err(|e| e.to_string())?.port();
            redirect_uri = format!("http://127.0.0.1:{port}/callback");
            let base_clone = base.clone();
            let state_clone = expected_state.clone();
            std::thread::spawn(move || {
                run_local_callback_server(listener, state_clone, tx, base_clone, audience);
            });
        }
        Err(e) => {
            bevy::log::warn!("Local callback bind failed ({e}), falling back to URI scheme");
            redirect_uri = match audience {
                LoginAudience::Studio => "vertexia-studio://auth",
                LoginAudience::Client => "vertexia://auth",
            }
            .to_string();
            let tx2 = tx.clone();
            let state2 = expected_state.clone();
            let base2 = base.clone();
            std::thread::spawn(move || {
                run_polling_only(state2, tx2, base2, audience);
            });
        }
    }
    let auth_url = format!(
        "{base}/studio/auth?state={}&redirect_uri={}&audience={}",
        urlencoding(&state),
        urlencoding(&redirect_uri),
        audience.as_str()
    );
    open_browser(&auth_url);

    flow.state = Some(state.clone());
    flow.redirect_uri = Some(redirect_uri);
    flow.receiver = Some(rx);
    flow.error = None;
    flow.waiting_since = Some(Instant::now());
    flow.is_polling = true;
    Ok(())
}

fn run_polling_only(
    expected_state: String,
    tx: Sender<Result<StoredCredentials, String>>,
    website_base: String,
    audience: LoginAudience,
) {
    let start = Instant::now();
    let timeout = Duration::from_secs(300);
    let mut last_poll = Instant::now() - Duration::from_secs(2);
    loop {
        if start.elapsed() > timeout {
            let _ = tx.send(Err("Authentication timed out (5 minutes)".to_string()));
            break;
        }
        if last_poll.elapsed() >= Duration::from_secs(2) {
            last_poll = Instant::now();
            if let Some(code) = poll_for_code(&expected_state, &website_base) {
                match exchange_code_with_base(&code, &website_base, audience) {
                    Ok(creds) => {
                        let _ = tx.send(Ok(creds));
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn poll_for_code(state: &str, base: &str) -> Option<String> {
    PlatformClient::with_origin(base)
        .ok()?
        .studio_poll(state)
        .ok()
        .flatten()
}

fn exchange_code_with_base(
    code: &str,
    base: &str,
    audience: LoginAudience,
) -> Result<StoredCredentials, String> {
    let client = PlatformClient::with_origin(base).map_err(|e| e.to_string())?;
    let login = client
        .exchange_code(code, audience)
        .map_err(|e| format!("exchange failed: {e}"))?;
    if audience == LoginAudience::Client && login.access_token.is_empty() {
        return Err("The platform did not issue an access token; update VERTEXIA.".to_string());
    }
    Ok(StoredCredentials::from_login(login))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn run_local_callback_server(
    listener: TcpListener,
    expected_state: String,
    tx: Sender<Result<StoredCredentials, String>>,
    website_base: String,
    audience: LoginAudience,
) {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    let timeout = Duration::from_secs(300);
    let mut last_poll = Instant::now();
    let mut polled_creds: Option<StoredCredentials> = None;
    let mut poll_success_time: Option<Instant> = None;
    loop {
        if start.elapsed() > timeout {
            if let Some(creds) = polled_creds.take() {
                let _ = tx.send(Ok(creds));
            } else {
                let _ = tx.send(Err("Authentication timed out (5 minutes)".to_string()));
            }
            break;
        }
        if let Some(t) = poll_success_time {
            if t.elapsed() > Duration::from_secs(10) {
                break;
            }
        }
        if polled_creds.is_none() && last_poll.elapsed() >= Duration::from_secs(2) {
            last_poll = Instant::now();
            if let Some(code) = poll_for_code(&expected_state, &website_base) {
                match exchange_code_with_base(&code, &website_base, audience) {
                    Ok(creds) => {
                        let _ = tx.send(Ok(creds.clone()));
                        polled_creds = Some(creds);
                        poll_success_time = Some(Instant::now());
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = req.lines().next().unwrap_or("");
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                if parts.len() < 2 {
                    continue;
                }
                let method = parts[0];
                let path = parts[1];
                if method == "OPTIONS" {
                    let resp = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    continue;
                }
                if path.starts_with("/favicon.ico") {
                    let html = "<html><body>not found</body></html>";
                    let resp = format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}",
                        html.as_bytes().len(),
                        html
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    continue;
                }
                let query = path.split('?').nth(1).unwrap_or("");
                let code_opt = parse_query_param(query, "code");
                let state_opt = parse_query_param(query, "state");
                let error_opt = parse_query_param(query, "error");

                if let Some(err) = error_opt {
                    let html = format!("<html><body style=\"font-family:sans-serif;text-align:center;padding:40px;\"><h2>Authorization denied</h2><p>{}</p><p>You may close this window.</p></body></html>", html_escape(&err));
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}", html.as_bytes().len(), html);
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    let _ = tx.send(Err(format!("Authorization denied: {err}")));
                    break;
                }

                if let Some(code) = code_opt {
                    let state = state_opt.unwrap_or_default();
                    if state != expected_state {
                        let html = "<html><body style=\"font-family:sans-serif;text-align:center;padding:40px;\"><h2>State mismatch</h2><p>Please try again from Studio.</p></body></html>";
                        let resp = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}", html.as_bytes().len(), html);
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                        let _ = tx.send(Err("State mismatch - please try again".to_string()));
                        break;
                    }
                    if let Some(creds) = polled_creds.clone() {
                        let html = format!("<html><head><meta charset=\"utf-8\"><title>VERTEXIA Studio</title></head><body style=\"font-family:sans-serif;text-align:center;padding:40px;background:#f5f5f8;\"><div style=\"max-width:480px;margin:0 auto;background:#fff;padding:32px;border-radius:12px;box-shadow:0 4px 24px rgba(0,0,0,0.08);\"><h2 style=\"color:#7423CB;margin:0 0 12px;\">Successfully authorized!</h2><p style=\"color:#444;\">You are now logged in as <b>{}</b>. You may close this window and return to VERTEXIA.</p><p style=\"margin-top:20px;\"><a href=\"#\" onclick=\"window.close();return false;\" style=\"display:inline-block;background:#7423CB;color:#fff;padding:10px 22px;border-radius:6px;text-decoration:none;font-weight:700;\">Close window</a></p><script>setTimeout(function(){{try{{window.close()}}catch(e){{}}}},1200);</script></div></body></html>", html_escape(&creds.username));
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}", html.as_bytes().len(), html);
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                        break;
                    }
                    let result = exchange_code_with_base(&code, &website_base, audience);

                    match result {
                        Ok(creds) => {
                            let html = format!("<html><head><meta charset=\"utf-8\"><title>VERTEXIA Studio</title></head><body style=\"font-family:sans-serif;text-align:center;padding:40px;background:#f5f5f8;\"><div style=\"max-width:480px;margin:0 auto;background:#fff;padding:32px;border-radius:12px;box-shadow:0 4px 24px rgba(0,0,0,0.08);\"><h2 style=\"color:#7423CB;margin:0 0 12px;\">Successfully authorized!</h2><p style=\"color:#444;\">You are now logged in as <b>{}</b>. You may close this window and return to VERTEXIA.</p><p style=\"margin-top:20px;\"><a href=\"#\" onclick=\"window.close();return false;\" style=\"display:inline-block;background:#7423CB;color:#fff;padding:10px 22px;border-radius:6px;text-decoration:none;font-weight:700;\">Close window</a></p><script>setTimeout(function(){{try{{window.close()}}catch(e){{}}}},1200);</script></div></body></html>", html_escape(&creds.username));
                            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}", html.as_bytes().len(), html);
                            let _ = stream.write_all(resp.as_bytes());
                            let _ = stream.flush();
                            let _ = tx.send(Ok(creds));
                            break;
                        }
                        Err(e) => {
                            let html = format!("<html><body style=\"font-family:sans-serif;text-align:center;padding:40px;\"><h2>Login failed</h2><p>{}</p></body></html>", html_escape(&e));
                            let resp = format!("HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/html\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}", html.as_bytes().len(), html);
                            let _ = stream.write_all(resp.as_bytes());
                            let _ = stream.flush();
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                } else {
                    let html = "<html><body style=\"font-family:sans-serif;text-align:center;padding:40px;\"><h2>Invalid request</h2><p>Missing code parameter.</p></body></html>";
                    let resp = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nConnection: close\r\n\r\n{}", html.as_bytes().len(), html);
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(80));
                continue;
            }
            Err(e) => {
                let _ = tx.send(Err(format!("listener error: {e}")));
                break;
            }
        }
    }
}

pub fn poll_studio_auth_flow(
    mut flow: ResMut<StudioAuthFlow>,
    mut store: ResMut<StudioAuthStore>,
    mut next_onboarding: ResMut<NextState<crate::studio::tools::OnboardingState>>,
    onboarding: Res<State<crate::studio::tools::OnboardingState>>,
) {
    if let Some(validation) = flow.validation.as_ref() {
        match validation.poll() {
            TaskPoll::Ready(Err(ApiError::Unauthorized)) => {
                warn!("Stored session was rejected by the platform; please log in again");
                forget_credentials(flow.audience);
                store.credentials = None;
                flow.validation = None;
                flow.error = Some(ApiError::Unauthorized.to_string());
                next_onboarding.set(crate::studio::tools::OnboardingState::Login);
            }
            TaskPoll::Ready(Err(e)) => {
                debug!("Could not verify the stored session yet: {e}");
                flow.validation = None;
            }
            TaskPoll::Ready(Ok(())) | TaskPoll::Abandoned => flow.validation = None,
            TaskPoll::Pending => {}
        }
    }

    if let Some(receiver) = flow.receiver.as_ref() {
        if let Ok(result) = receiver.try_recv() {
            match result {
                Ok(creds) => {
                    if let Err(e) = save_credentials(flow.audience, &creds) {
                        warn!("{e}");
                    }
                    store.credentials = Some(creds);
                    flow.error = None;
                    flow.state = None;
                    flow.receiver = None;
                    flow.waiting_since = None;
                    flow.is_polling = false;
                    if *onboarding.get() == crate::studio::tools::OnboardingState::Login {
                        next_onboarding.set(crate::studio::tools::OnboardingState::Inactive);
                    }
                }
                Err(e) => {
                    flow.error = Some(e);
                    flow.receiver = None;
                    flow.state = None;
                    flow.is_polling = false;
                    flow.waiting_since = None;
                }
            }
            return;
        }
    }

    if flow.is_polling {
        if let Some(start) = flow.waiting_since {
            if start.elapsed() > Duration::from_secs(300) {
                flow.error = Some("Authentication timed out".to_string());
                flow.state = None;
                flow.receiver = None;
                flow.is_polling = false;
                flow.waiting_since = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vertexia-auth-test-{name}-{}-{}",
            std::process::id(),
            generate_state()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn creds() -> StoredCredentials {
        StoredCredentials {
            username: "wempo".into(),
            uid: 5,
            access_token: "opaque-token".into(),
            expires_at: Some(unix_now() + 3600),
            ukey: String::new(),
        }
    }

    #[test]
    fn token_file_round_trips_with_private_permissions() {
        let dir = temp_dir("perm");
        let path = dir.join("nested").join("client_auth.json");
        save_credentials_to(&path, &creds()).unwrap();
        assert_eq!(load_credentials_from(&path), Some(creds()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be 0600, got {mode:o}");
            let dir_mode =
                std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "config dir must be 0700, got {dir_mode:o}");
        }
        // Overwriting keeps 0600 and leaves no temp files behind.
        let mut updated = creds();
        updated.access_token = "rotated".into();
        save_credentials_to(&path, &updated).unwrap();
        assert_eq!(load_credentials_from(&path).unwrap().access_token, "rotated");
        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name() != "client_auth.json")
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn loading_tightens_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("loose");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("studio_auth.json");
        std::fs::write(&path, serde_json::to_vec(&creds()).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_credentials_from(&path).is_some());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_or_empty_tokens_are_ignored() {
        let dir = temp_dir("expired");
        let path = dir.join("client_auth.json");
        let mut expired = creds();
        expired.expires_at = Some(unix_now() - 1);
        save_credentials_to(&path, &expired).unwrap();
        assert_eq!(load_credentials_from(&path), None);
        let mut empty = creds();
        empty.access_token.clear();
        save_credentials_to(&path, &empty).unwrap();
        assert_eq!(load_credentials_from(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let mut c = creds();
        c.ukey = "legacy-secret".into();
        let out = format!("{c:?}");
        assert!(!out.contains("opaque-token"));
        assert!(!out.contains("legacy-secret"));
    }

    #[test]
    fn legacy_files_deserialize() {
        let legacy: StoredCredentials =
            serde_json::from_str(r#"{"username":"w","uid":1,"ukey":"k"}"#).unwrap();
        assert!(legacy.is_usable());
        assert!(legacy.bearer().is_none());
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(html_escape("<b>\"x\"&'"), "&lt;b&gt;&quot;x&quot;&amp;&#39;");
    }
}
