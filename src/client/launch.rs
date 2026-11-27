//! How the native client was asked to start: browse games, join a specific game, or (dev only)
//! connect straight to a server with a locally generated connect token.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::Deserialize;

use crate::client::uri::{ClientLaunchUri, parse_client_launch_uri};

pub const DEV_LEGACY_CONNECT_ENV: &str = "VERTEXIA_DEV_LEGACY_CONNECT";
pub const DEV_LEGACY_CONNECT_FLAG: &str = "--dev-legacy-connect";

#[derive(Debug, Clone, PartialEq)]
pub enum LaunchTarget {
    /// Log in if needed, then show the game browser.
    Browse,
    /// Log in if needed, then join this game.
    Game(u64),
    /// Dev only: build a connect token locally with the shared netcode key and send the legacy
    /// ukey hello. Requires a server started with `--dev --legacy-ukey-auth`.
    LegacyDirect(LegacyConnect),
}

#[derive(Clone, PartialEq)]
pub struct LegacyConnect {
    pub addr: SocketAddr,
    pub ukey: String,
    pub netcode_key: [u8; 32],
    pub protocol_id: u64,
}

impl std::fmt::Debug for LegacyConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegacyConnect")
            .field("addr", &self.addr)
            .field("ukey", &"<redacted>")
            .field("netcode_key", &"<redacted>")
            .field("protocol_id", &self.protocol_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientLaunchConfig {
    pub target: LaunchTarget,
    pub no_clouds: bool,
    /// Shown in the session UI, e.g. when an outdated link was ignored.
    pub notice: Option<String>,
}

const OUTDATED_LINK: &str =
    "This link uses the old direct-connect format. Pick the game from the list instead.";

fn truthy(value: Option<String>) -> bool {
    value.is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
}

fn parse_game_id(raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| format!("invalid game id: {raw}"))
}

/// Parses the client command line. `args[0]` is the executable. `env` reads environment
/// variables, so tests can inject them.
pub fn parse(
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
) -> Result<ClientLaunchConfig, String> {
    let dev_legacy =
        args.iter().any(|a| a == DEV_LEGACY_CONNECT_FLAG) || truthy(env(DEV_LEGACY_CONNECT_ENV));

    let mut game: Option<u64> = None;
    let mut no_clouds = false;
    let mut notice = None;
    let mut legacy_uri = None;
    let mut ip: Option<IpAddr> = None;
    let mut port: Option<u16> = None;
    let mut ukey: Option<String> = None;
    let mut netcode_key: Option<String> = None;
    let mut protocol_id: Option<u64> = None;
    let mut legacy_flags: Vec<&str> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        let value = args.get(i + 1).map(String::as_str);
        let mut take_value = |name: &str| {
            i += 1;
            value.ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "--no-clouds" | "--disable-clouds" => no_clouds = true,
            DEV_LEGACY_CONNECT_FLAG => {}
            "--game" | "--game-id" => game = Some(parse_game_id(take_value(arg)?)?),
            "--ip" => {
                let v = take_value(arg)?;
                ip = Some(v.parse().map_err(|_| format!("invalid --ip: {v}"))?);
                legacy_flags.push("--ip");
            }
            "--port" => {
                let v = take_value(arg)?;
                port = Some(v.parse().map_err(|_| format!("invalid --port: {v}"))?);
                legacy_flags.push("--port");
            }
            "--ukey" => {
                ukey = Some(take_value(arg)?.to_string());
                legacy_flags.push("--ukey");
            }
            "--netcode-key" => {
                netcode_key = Some(take_value(arg)?.to_string());
                legacy_flags.push("--netcode-key");
            }
            "--protocol-id" => {
                let v = take_value(arg)?;
                protocol_id = Some(
                    v.parse()
                        .map_err(|_| format!("invalid --protocol-id: {v}"))?,
                );
                legacy_flags.push("--protocol-id");
            }
            _ => match parse_client_launch_uri(arg) {
                Some(ClientLaunchUri::Game(id)) => game = Some(id),
                Some(ClientLaunchUri::Legacy(info)) => legacy_uri = Some(info),
                None => {}
            },
        }
        i += 1;
    }

    if !dev_legacy {
        if !legacy_flags.is_empty() {
            return Err(format!(
                "{}: only supported with {DEV_LEGACY_CONNECT_FLAG}; use --game <id> to join through VERTEXIA",
                legacy_flags.join(", ")
            ));
        }
        if legacy_uri.is_some() && game.is_none() {
            notice = Some(OUTDATED_LINK.to_string());
        }
        let target = game.map_or(LaunchTarget::Browse, LaunchTarget::Game);
        return Ok(ClientLaunchConfig {
            target,
            no_clouds,
            notice,
        });
    }

    if game.is_some() {
        return Err(format!(
            "--game cannot be combined with {DEV_LEGACY_CONNECT_FLAG}"
        ));
    }
    if let Some(info) = legacy_uri {
        ip = ip.or(Some(info.ip));
        port = port.or(Some(info.port));
        ukey = ukey.or(Some(info.ukey));
    }
    let env_key = env(crate::common::net::netcode::NETCODE_PRIVATE_KEY_ENV);
    let env_protocol = env(crate::common::net::netcode::NETCODE_PROTOCOL_ID_ENV);
    if let Some(key) = &netcode_key {
        if crate::common::net::netcode::parse_hex_key(key).is_none() {
            return Err("--netcode-key must be exactly 64 hex characters".to_string());
        }
    }
    let (netcode_key, protocol_id) = crate::common::net::netcode::load_server_netcode_secrets(
        false,
        env_key.as_deref(),
        env_protocol.as_deref(),
        netcode_key.as_deref(),
        protocol_id,
    )?;
    Ok(ClientLaunchConfig {
        target: LaunchTarget::LegacyDirect(LegacyConnect {
            addr: SocketAddr::new(
                ip.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                port.unwrap_or(5000),
            ),
            ukey: ukey.unwrap_or_default(),
            netcode_key,
            protocol_id,
        }),
        no_clouds,
        notice,
    })
}

#[derive(Deserialize)]
struct LaunchInfo {
    #[serde(default)]
    game_id: Option<u64>,
}

/// Reads a game id from a `launch_info.json` left by a launcher, deleting the file afterwards
/// (older launchers put a ukey in it).
pub fn take_launch_info_game() -> Option<u64> {
    let candidates = [
        crate::common::assets_path::resolve_launch_info_path(),
        std::path::PathBuf::from("launch_info.json"),
    ];
    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let _ = std::fs::remove_file(&path);
        if let Some(id) = parse_launch_info(&content) {
            return Some(id);
        }
    }
    None
}

fn parse_launch_info(content: &str) -> Option<u64> {
    serde_json::from_str::<LaunchInfo>(content)
        .ok()?
        .game_id
        .filter(|id| *id > 0)
}

/// Parses the real process arguments and environment.
pub fn from_env_and_args() -> Result<ClientLaunchConfig, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut config = parse(&args, |k| std::env::var(k).ok())?;
    if config.target == LaunchTarget::Browse {
        if let Some(id) = take_launch_info_game() {
            config.target = LaunchTarget::Game(id);
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("client")
            .chain(list.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_to_browsing() {
        let config = parse(&args(&[]), no_env).unwrap();
        assert_eq!(config.target, LaunchTarget::Browse);
        assert!(!config.no_clouds);
        assert_eq!(config.notice, None);
    }

    #[test]
    fn parses_game_flag_and_uri() {
        assert_eq!(
            parse(&args(&["--game", "12"]), no_env).unwrap().target,
            LaunchTarget::Game(12)
        );
        assert_eq!(
            parse(&args(&["--game-id", "13", "--no-clouds"]), no_env)
                .unwrap()
                .target,
            LaunchTarget::Game(13)
        );
        assert_eq!(
            parse(&args(&["vertexia-client://play?game=14"]), no_env)
                .unwrap()
                .target,
            LaunchTarget::Game(14)
        );
        assert!(parse(&args(&["--game", "0"]), no_env).is_err());
        assert!(parse(&args(&["--game"]), no_env).is_err());
    }

    #[test]
    fn legacy_flags_need_dev_mode() {
        let err = parse(&args(&["--ip", "127.0.0.1", "--ukey", "k"]), no_env).unwrap_err();
        assert!(err.contains(DEV_LEGACY_CONNECT_FLAG), "{err}");
        assert!(parse(&args(&["--netcode-key", &"ab".repeat(32)]), no_env).is_err());
    }

    #[test]
    fn legacy_links_fall_back_to_browsing_with_a_notice() {
        let config = parse(
            &args(&["vertexia-client://play?ip=127.0.0.1&port=5000&ukey=k"]),
            no_env,
        )
        .unwrap();
        assert_eq!(config.target, LaunchTarget::Browse);
        assert!(config.notice.is_some());
    }

    #[test]
    fn dev_legacy_connect_builds_a_direct_target() {
        let key = "ab".repeat(32);
        let config = parse(
            &args(&[
                DEV_LEGACY_CONNECT_FLAG,
                "--ip",
                "10.0.0.5",
                "--port",
                "6000",
                "--ukey",
                "k",
                "--netcode-key",
                &key,
                "--protocol-id",
                "7",
            ]),
            no_env,
        )
        .unwrap();
        let LaunchTarget::LegacyDirect(legacy) = config.target else {
            panic!("expected legacy target");
        };
        assert_eq!(legacy.addr, "10.0.0.5:6000".parse().unwrap());
        assert_eq!(legacy.ukey, "k");
        assert_eq!(legacy.netcode_key, [0xab; 32]);
        assert_eq!(legacy.protocol_id, 7);
        assert!(!format!("{legacy:?}").contains("abab"));
    }

    #[test]
    fn dev_legacy_connect_reads_env_and_uri() {
        let key = "cd".repeat(32);
        let env = |k: &str| match k {
            DEV_LEGACY_CONNECT_ENV => Some("1".to_string()),
            "NETCODE_PRIVATE_KEY" => Some(key.clone()),
            "NETCODE_PROTOCOL_ID" => Some("99".to_string()),
            _ => None,
        };
        let config = parse(
            &args(&["vertexia://join?ip=127.0.0.1&port=5001&ukey=u"]),
            env,
        )
        .unwrap();
        let LaunchTarget::LegacyDirect(legacy) = config.target else {
            panic!("expected legacy target");
        };
        assert_eq!(legacy.addr.port(), 5001);
        assert_eq!(legacy.ukey, "u");
        assert_eq!(legacy.netcode_key, [0xcd; 32]);
        assert_eq!(legacy.protocol_id, 99);
    }

    #[test]
    fn dev_legacy_connect_rejects_bad_input() {
        assert!(
            parse(
                &args(&[DEV_LEGACY_CONNECT_FLAG, "--netcode-key", "zz"]),
                no_env
            )
            .is_err()
        );
        assert!(parse(&args(&[DEV_LEGACY_CONNECT_FLAG, "--game", "3"]), no_env).is_err());
    }

    #[test]
    fn launch_info_only_carries_a_game_id() {
        assert_eq!(parse_launch_info(r#"{"game_id": 8}"#), Some(8));
        assert_eq!(parse_launch_info(r#"{"game_id": 0}"#), None);
        assert_eq!(
            parse_launch_info(r#"{"ip":"1.2.3.4","port":5000,"ukey":"k"}"#),
            None
        );
        assert_eq!(parse_launch_info("not json"), None);
    }
}
