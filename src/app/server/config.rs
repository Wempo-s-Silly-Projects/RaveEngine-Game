use std::net::{IpAddr, Ipv4Addr};

pub struct ServerAppConfig {
    pub port: u16,
    pub map_path: String,
    pub bind_addr: IpAddr,
    pub netcode_key: [u8; 32],
    pub protocol_id: u64,
    pub allow_unauthenticated: bool,
    /// Accept legacy HelloMessage ukeys (local development and migration only).
    pub legacy_ukey_auth: bool,
    pub server_id: Option<String>,
    pub game_id: Option<u64>,
    pub platform: Option<crate::server::platform::PlatformReporterConfig>,
}

impl ServerAppConfig {
    /// In-process Studio playtest server: random key, loopback only, no platform reporting.
    pub fn playtest(
        port: u16,
        map_path: String,
        netcode_key: [u8; 32],
        protocol_id: u64,
    ) -> Self {
        Self {
            port,
            map_path,
            bind_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            netcode_key,
            protocol_id,
            allow_unauthenticated: true,
            legacy_ukey_auth: true,
            server_id: None,
            game_id: None,
            platform: None,
        }
    }

    pub fn into_plugin(self) -> crate::server::ServerPlugin {
        crate::server::ServerPlugin {
            map_path: self.map_path,
            port: self.port,
            bind_addr: self.bind_addr,
            netcode_key: self.netcode_key,
            protocol_id: self.protocol_id,
            allow_unauthenticated: self.allow_unauthenticated,
            legacy_ukey_auth: self.legacy_ukey_auth,
            server_id: self.server_id,
            platform: self.platform,
        }
    }

    /// Dedicated server configuration.
    ///
    /// Production is the default. Pass `--dev` (or `VERTEXIA_DEV=1`) for local development,
    /// which allows CLI netcode secrets, the all-zero dev key and `--allow-unauthenticated`.
    pub fn from_env_and_args() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().collect();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self::parse(&args, env)
    }

    pub fn parse(
        args: &[String],
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut port = 5000; //default
        let mut map_path = crate::common::assets_path::resolve_vrtx_path("assets/maps/default.vrtx")
            .to_string_lossy()
            .into_owned();
        let mut bind_addr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let mut netcode_key_cli: Option<String> = None;
        let mut protocol_id_cli: Option<u64> = None;
        let mut allow_unauthenticated = false;
        let mut legacy_ukey_auth = env("VERTEXIA_LEGACY_UKEY_AUTH").is_some_and(|v| v == "1");
        let mut dev = env("VERTEXIA_DEV").is_some_and(|v| v == "1");
        let mut server_id = env("VERTEXIA_SERVER_ID").or_else(|| env("VERTIGO_SERVER_ID"));
        let mut game_id = env("VERTEXIA_GAME_ID")
            .or_else(|| env("VERTIGO_GAME_ID"))
            .and_then(|v| v.trim().parse().ok());

        for i in 0..args.len() {
            let next = args.get(i + 1);
            match args[i].as_str() {
                "--port" => {
                    if let Some(p) = next.and_then(|v| v.parse::<u16>().ok()) {
                        port = p;
                    }
                }
                "--map" => {
                    if let Some(v) = next {
                        map_path = v.clone();
                    }
                }
                "--bind" => {
                    if let Some(ip) = next.and_then(|v| v.parse::<IpAddr>().ok()) {
                        bind_addr = ip;
                    }
                }
                "--netcode-key" => netcode_key_cli = next.cloned(),
                "--protocol-id" => protocol_id_cli = next.and_then(|v| v.parse::<u64>().ok()),
                "--allow-unauthenticated" => allow_unauthenticated = true,
                "--legacy-ukey-auth" => legacy_ukey_auth = true,
                "--dev" => dev = true,
                "--server-id" => server_id = next.cloned().filter(|v| !v.trim().is_empty()),
                "--game-id" => game_id = next.and_then(|v| v.trim().parse().ok()),
                _ => {}
            }
        }

        let production = !dev;
        if production && allow_unauthenticated {
            return Err("--allow-unauthenticated requires --dev".to_string());
        }
        let (netcode_key, protocol_id) =
            crate::common::net::netcode::load_server_netcode_secrets(
                production,
                env(crate::common::net::netcode::NETCODE_PRIVATE_KEY_ENV).as_deref(),
                env(crate::common::net::netcode::NETCODE_PROTOCOL_ID_ENV).as_deref(),
                netcode_key_cli.as_deref(),
                protocol_id_cli,
            )?;
        let platform =
            crate::server::platform::PlatformReporterConfig::from_env(server_id.as_deref());

        Ok(Self {
            port,
            map_path,
            bind_addr,
            netcode_key,
            protocol_id,
            allow_unauthenticated,
            legacy_ukey_auth,
            server_id,
            game_id,
            platform,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("RaveEngineServer")
            .chain(list.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn production_fails_without_netcode_env() {
        let err = ServerAppConfig::parse(&args(&["--port", "6000"]), |_| None)
            .err()
            .unwrap();
        assert!(err.contains("NETCODE_PRIVATE_KEY"));
    }

    #[test]
    fn production_reads_platform_launch_arguments() {
        let key = "ab".repeat(32);
        let cfg = ServerAppConfig::parse(
            &args(&[
                "--port",
                "27015",
                "--bind",
                "0.0.0.0",
                "--game-id",
                "12",
                "--server-id",
                "aB3dE5gH",
            ]),
            |k| match k {
                "NETCODE_PRIVATE_KEY" => Some(key.clone()),
                "NETCODE_PROTOCOL_ID" => Some("77".into()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(cfg.port, 27015);
        assert_eq!(cfg.protocol_id, 77);
        assert_eq!(cfg.netcode_key, [0xab; 32]);
        assert_eq!(cfg.server_id.as_deref(), Some("aB3dE5gH"));
        assert_eq!(cfg.game_id, Some(12));
        assert!(!cfg.legacy_ukey_auth);
        assert!(!cfg.allow_unauthenticated);
    }

    #[test]
    fn unauthenticated_requires_dev() {
        let key = "ab".repeat(32);
        let env = |k: &str| match k {
            "NETCODE_PRIVATE_KEY" => Some(key.clone()),
            "NETCODE_PROTOCOL_ID" => Some("1".into()),
            _ => None,
        };
        assert!(ServerAppConfig::parse(&args(&["--allow-unauthenticated"]), env).is_err());
        let cfg =
            ServerAppConfig::parse(&args(&["--dev", "--allow-unauthenticated"]), |_| None)
                .unwrap();
        assert!(cfg.allow_unauthenticated);
        assert_eq!(cfg.netcode_key, [0; 32]);
    }
}
