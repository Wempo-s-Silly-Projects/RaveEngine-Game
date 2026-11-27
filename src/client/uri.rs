use std::net::IpAddr;

pub const CLIENT_URI_SCHEME: &str = "vertexia-client";
pub const CLIENT_URI_FALLBACK_SCHEME: &str = "vertexia";

/// Legacy direct-connect link (`?ip=&port=&ukey=`). Only honoured with `--dev-legacy-connect`.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientJoinInfo {
    pub ip: IpAddr,
    pub port: u16,
    pub ukey: String,
}

impl std::fmt::Debug for ClientJoinInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientJoinInfo")
            .field("ip", &self.ip)
            .field("port", &self.port)
            .field("ukey", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientLaunchUri {
    /// `vertexia-client://play?game=<id>` or `vertexia://join?game_id=<id>`.
    Game(u64),
    Legacy(ClientJoinInfo),
}

/// Link the website uses to open a game in the client.
pub fn build_client_join_uri(game_id: u64) -> String {
    format!("{CLIENT_URI_SCHEME}://play?game={game_id}")
}

pub fn parse_client_launch_uri(raw: &str) -> Option<ClientLaunchUri> {
    let query = launch_query(raw)?;
    if let Some(id) = query_param(query, "game").or_else(|| query_param(query, "game_id")) {
        return id
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .map(ClientLaunchUri::Game);
    }
    parse_legacy_join(query).map(ClientLaunchUri::Legacy)
}

pub fn extract_client_launch_from_args(args: &[String]) -> Option<ClientLaunchUri> {
    args.iter().skip(1).find_map(|a| parse_client_launch_uri(a))
}

/// Returns the query string of a `play`/`join` link in either client scheme.
fn launch_query(raw: &str) -> Option<&str> {
    let arg = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();
    let lower = arg.to_lowercase();
    let rest = if lower.starts_with(&format!("{CLIENT_URI_SCHEME}://")) {
        &arg[CLIENT_URI_SCHEME.len() + 3..]
    } else if lower.starts_with(&format!("{CLIENT_URI_FALLBACK_SCHEME}://")) {
        &arg[CLIENT_URI_FALLBACK_SCHEME.len() + 3..]
    } else {
        return None;
    };
    let (path, query) = rest.split_once('?')?;
    let action = path.trim().trim_matches('/').to_lowercase();
    (action == "play" || action == "join").then_some(query)
}

fn parse_legacy_join(query: &str) -> Option<ClientJoinInfo> {
    let host = query_param(query, "ip").or_else(|| query_param(query, "host"))?;
    let port_str = query_param(query, "port")?;
    let ukey = query_param(query, "ukey")?;
    if host.is_empty() || ukey.is_empty() {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    let ip: IpAddr = host.parse().or_else(|_| resolve_host(&host, port)).ok()?;
    Some(ClientJoinInfo { ip, port, ukey })
}

#[cfg(target_os = "windows")]
pub fn register_client_uri_scheme() -> Result<(), String> {
    use std::process::Command;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_str = exe.display().to_string();
    let ps_script = format!(
        "$p='HKCU:\\Software\\Classes\\vertexia-client'; if(!(Test-Path $p)){{New-Item -Path $p -Force | Out-Null}}; Set-ItemProperty -Path $p -Name '(Default)' -Value 'URL:VERTEXIA Client'; Set-ItemProperty -Path $p -Name 'URL Protocol' -Value ''; $c=\"$p\\shell\\open\\command\"; if(!(Test-Path $c)){{New-Item -Path $c -Force | Out-Null}}; Set-ItemProperty -Path $c -Name '(Default)' -Value '\"{exe_str}\" \"%1\"'",
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_script])
        .output();
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn register_client_uri_scheme() -> Result<(), String> {
    Ok(())
}

fn resolve_host(host: &str, port: u16) -> Result<IpAddr, ()> {
    use std::net::ToSocketAddrs;
    format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|_| ())?
        .find_map(|a| Some(a.ip()))
        .ok_or(())
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut iter = pair.splitn(2, '=');
        let k = iter.next()?;
        let v = iter.next().unwrap_or("");
        if k.eq_ignore_ascii_case(key) {
            return Some(uri_decode(v));
        }
    }
    None
}

pub fn uri_encode(s: &str) -> String {
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

fn uri_decode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(raw: &str) -> ClientJoinInfo {
        match parse_client_launch_uri(raw) {
            Some(ClientLaunchUri::Legacy(info)) => info,
            other => panic!("expected legacy join, got {other:?}"),
        }
    }

    #[test]
    fn parses_game_links() {
        assert_eq!(
            parse_client_launch_uri("vertexia-client://play?game=42"),
            Some(ClientLaunchUri::Game(42))
        );
        assert_eq!(
            parse_client_launch_uri("vertexia://join?game_id=7"),
            Some(ClientLaunchUri::Game(7))
        );
        assert_eq!(
            parse_client_launch_uri("\"vertexia-client://play/?game_id=9&utm=x\""),
            Some(ClientLaunchUri::Game(9))
        );
    }

    #[test]
    fn rejects_bad_game_ids() {
        assert_eq!(parse_client_launch_uri("vertexia-client://play?game=0"), None);
        assert_eq!(parse_client_launch_uri("vertexia-client://play?game=-1"), None);
        assert_eq!(parse_client_launch_uri("vertexia-client://play?game=abc"), None);
        // A bad game id must not fall through to the legacy form.
        assert_eq!(
            parse_client_launch_uri("vertexia-client://play?game=x&ip=127.0.0.1&port=1&ukey=k"),
            None
        );
    }

    #[test]
    fn build_parse_round_trip() {
        assert_eq!(
            parse_client_launch_uri(&build_client_join_uri(1234)),
            Some(ClientLaunchUri::Game(1234))
        );
    }

    #[test]
    fn parses_legacy_play_uri() {
        let info = legacy("vertexia-client://play?ip=127.0.0.1&port=5000&ukey=abc123");
        assert_eq!(info.ip.to_string(), "127.0.0.1");
        assert_eq!(info.port, 5000);
        assert_eq!(info.ukey, "abc123");
        let info = legacy("vertexia://join?ip=10.0.0.2&port=6000&ukey=k1");
        assert_eq!(info.ip.to_string(), "10.0.0.2");
        assert!(!format!("{info:?}").contains("k1"));
    }

    #[test]
    fn rejects_auth_and_incomplete_uris() {
        for uri in [
            "vertexia-studio://auth?code=abc&state=xyz",
            "vertexia://auth?code=abc",
            "vertexia-client://auth?code=abc",
            "vertexia-client://play?ip=127.0.0.1&port=5000",
            "vertexia-client://play?port=5000&ukey=k",
            "vertexia-client://play?ip=999.1.1.1&port=5000&ukey=k",
            "vertexia-client://play?ip=127.0.0.1&port=abc&ukey=k",
            "vertexia-client://play",
            "https://example.com/play?game=1",
        ] {
            assert_eq!(parse_client_launch_uri(uri), None, "{uri}");
        }
    }

    #[test]
    fn extracts_launch_from_arg_list() {
        let args = vec![
            "RaveEngineClient.exe".to_string(),
            "vertexia-client://play?game=5".to_string(),
        ];
        assert_eq!(
            extract_client_launch_from_args(&args),
            Some(ClientLaunchUri::Game(5))
        );
        let args = vec!["RaveEngineClient.exe".to_string(), "--ip".to_string()];
        assert_eq!(extract_client_launch_from_args(&args), None);
    }
}
