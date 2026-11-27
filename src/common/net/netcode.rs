use bevy::log::warn;

pub fn parse_hex_key(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

pub fn format_hex_key(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

pub const NETCODE_PRIVATE_KEY_ENV: &str = "NETCODE_PRIVATE_KEY";
pub const NETCODE_PROTOCOL_ID_ENV: &str = "NETCODE_PROTOCOL_ID";

/// Loads the netcode secrets for a dedicated server.
///
/// Production servers must receive both values from the environment (never the command line,
/// which is visible to other local users): a 64-hex-char private key that is not all zeros and
/// a u64 protocol id. Development servers may fall back to CLI flags and insecure defaults.
pub fn load_server_netcode_secrets(
    production: bool,
    env_key: Option<&str>,
    env_protocol: Option<&str>,
    cli_key: Option<&str>,
    cli_protocol: Option<u64>,
) -> Result<([u8; 32], u64), String> {
    if production {
        if cli_key.is_some() || cli_protocol.is_some() {
            warn!("--netcode-key/--protocol-id are ignored in production; use the environment");
        }
        let key = env_key
            .ok_or_else(|| format!("{NETCODE_PRIVATE_KEY_ENV} is required in production"))
            .and_then(|v| {
                parse_hex_key(v).ok_or_else(|| {
                    format!("{NETCODE_PRIVATE_KEY_ENV} must be exactly 64 hex characters")
                })
            })?;
        if key == [0u8; 32] {
            return Err(format!("{NETCODE_PRIVATE_KEY_ENV} must not be all zeros"));
        }
        let protocol_id = env_protocol
            .ok_or_else(|| format!("{NETCODE_PROTOCOL_ID_ENV} is required in production"))?
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{NETCODE_PROTOCOL_ID_ENV} must be an unsigned 64-bit integer"))?;
        return Ok((key, protocol_id));
    }
    let key = cli_key
        .and_then(parse_hex_key)
        .or_else(|| env_key.and_then(parse_hex_key))
        .unwrap_or_else(|| {
            warn!("{NETCODE_PRIVATE_KEY_ENV} not set, defaulting to an all-zero dev key");
            [0u8; 32]
        });
    let protocol_id = cli_protocol
        .or_else(|| env_protocol.and_then(|v| v.trim().parse::<u64>().ok()))
        .unwrap_or(0);
    Ok((key, protocol_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_key_accepts_exactly_64_hex_chars() {
        let key = parse_hex_key(&"ab".repeat(32)).expect("valid 64-char hex key");
        assert_eq!(key[0], 0xab);
        assert_eq!(key[31], 0xab);
    }

    #[test]
    fn parse_hex_key_rejects_bad_lengths_and_garbage() {
        assert!(parse_hex_key("").is_none());
        assert!(parse_hex_key(&"ab".repeat(31)).is_none());
        assert!(parse_hex_key(&"zz".repeat(32)).is_none());
        assert!(
            parse_hex_key(&"AB".repeat(32)).is_some(),
            "uppercase hex is fine"
        );
    }

    #[test]
    fn format_hex_key_round_trips() {
        let key = parse_hex_key(&"42".repeat(32)).unwrap();
        assert_eq!(format_hex_key(&key), "42".repeat(32));
    }


    #[test]
    fn production_requires_env_secrets() {
        let key = "ab".repeat(32);
        assert_eq!(
            load_server_netcode_secrets(true, Some(&key), Some("42"), None, None).unwrap(),
            ([0xab; 32], 42)
        );
        assert!(load_server_netcode_secrets(true, None, Some("42"), Some(&key), None).is_err());
        assert!(load_server_netcode_secrets(true, Some(&key), None, None, Some(1)).is_err());
        assert!(load_server_netcode_secrets(true, Some("abc"), Some("1"), None, None).is_err());
        assert!(
            load_server_netcode_secrets(true, Some(&"00".repeat(32)), Some("1"), None, None)
                .is_err()
        );
        assert!(load_server_netcode_secrets(true, Some(&key), Some("-1"), None, None).is_err());
    }

    #[test]
    fn development_allows_cli_and_defaults() {
        let key = "cd".repeat(32);
        assert_eq!(
            load_server_netcode_secrets(false, None, None, Some(&key), Some(9)).unwrap(),
            ([0xcd; 32], 9)
        );
        assert_eq!(
            load_server_netcode_secrets(false, None, None, None, None).unwrap(),
            ([0; 32], 0)
        );
    }
}
