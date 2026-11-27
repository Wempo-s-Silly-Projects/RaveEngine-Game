//! Platform-issued netcode connect tokens and the VERTEXIA user_data layout.
//!
//! Byte layout (as implemented by lightyear_netcode 0.28, confirmed from its source; note that
//! lightyear writes every integer BIG-endian, unlike the little-endian netcode.io reference):
//!
//! ```text
//! public token (2048 bytes, zero padded)
//!   0    13  version "NETCODE 1.02\0"
//!   13   8   protocol_id            u64 BE
//!   21   8   create_timestamp       u64 BE (unix seconds)
//!   29   8   expire_timestamp       u64 BE
//!   37   24  nonce                  XChaCha20 nonce
//!   61   1024 encrypted private data (1008 bytes ciphertext + 16 byte Poly1305 tag)
//!   1085 4   timeout_seconds        i32 BE
//!   1089 4   address count          u32 BE, then per address:
//!            u8 type (1 = IPv4, 2 = IPv6), 4|16 address bytes, u16 BE port
//!   ...  32  client_to_server_key
//!   ...  32  server_to_client_key
//! private data plaintext (1008 bytes, zero padded), XChaCha20-Poly1305 encrypted with
//! the 32-byte NETCODE_PRIVATE_KEY, the token nonce and associated data
//! version(13) || protocol_id u64 BE || expire_timestamp u64 BE:
//!   client_id u64 BE, timeout_seconds i32 BE, address list (same encoding),
//!   client_to_server_key(32), server_to_client_key(32), user_data(256)
//! ```
//!
//! VERTEXIA user_data (256 bytes):
//!
//! ```text
//!   0   8   user id                 u64 little-endian (must equal client_id)
//!   8   32  gameserver id           ASCII, NUL padded (platform ids are 8 alphanumerics)
//!   40  1   username length N       optional, 0 = absent
//!   41  N   username                UTF-8, N <= 64
//!   ... zero
//! ```

use base64::Engine;
use lightyear::netcode::ConnectToken;

pub const CONNECT_TOKEN_BYTES: usize = 2048;
pub const USER_DATA_BYTES: usize = 256;
const SERVER_ID_OFFSET: usize = 8;
const SERVER_ID_LEN: usize = 32;
const USERNAME_OFFSET: usize = SERVER_ID_OFFSET + SERVER_ID_LEN;
pub const USERNAME_MAX_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Base64(String),
    Length(usize),
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Base64(e) => write!(f, "connect token is not valid base64: {e}"),
            TokenError::Length(n) => {
                write!(
                    f,
                    "connect token must be {CONNECT_TOKEN_BYTES} bytes, got {n}"
                )
            }
            TokenError::Invalid(e) => write!(f, "connect token is malformed: {e}"),
        }
    }
}

impl std::error::Error for TokenError {}

pub fn decode_token_bytes(b64: &str) -> Result<Vec<u8>, TokenError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| TokenError::Base64(e.to_string()))?;
    if bytes.len() != CONNECT_TOKEN_BYTES {
        return Err(TokenError::Length(bytes.len()));
    }
    Ok(bytes)
}

/// Decodes the `connect_token` field of a `/join` response.
pub fn decode_connect_token(b64: &str) -> Result<ConnectToken, TokenError> {
    let bytes = decode_token_bytes(b64)?;
    ConnectToken::try_from_bytes(&bytes).map_err(|e| TokenError::Invalid(e.to_string()))
}

/// Unix expiry of a serialized token (public header, not authenticated by the client).
pub fn token_expire_timestamp(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(29..37)?.try_into().ok()?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIdentity {
    pub user_id: u64,
    pub server_id: Option<String>,
    pub username: Option<String>,
}

pub fn encode_user_data(
    user_id: u64,
    server_id: &str,
    username: Option<&str>,
) -> [u8; USER_DATA_BYTES] {
    let mut data = [0u8; USER_DATA_BYTES];
    data[..8].copy_from_slice(&user_id.to_le_bytes());
    let sid = server_id.as_bytes();
    let n = sid.len().min(SERVER_ID_LEN);
    data[SERVER_ID_OFFSET..SERVER_ID_OFFSET + n].copy_from_slice(&sid[..n]);
    if let Some(name) = username {
        let mut end = name.len().min(USERNAME_MAX_BYTES);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        data[USERNAME_OFFSET] = end as u8;
        data[USERNAME_OFFSET + 1..USERNAME_OFFSET + 1 + end]
            .copy_from_slice(&name.as_bytes()[..end]);
    }
    data
}

/// Reads the identity the platform sealed into the token. Server-side only: the bytes come
/// from the decrypted private part, so they are authenticated by NETCODE_PRIVATE_KEY.
pub fn parse_user_data(data: &[u8; USER_DATA_BYTES]) -> TokenIdentity {
    let user_id = u64::from_le_bytes(data[..8].try_into().unwrap_or_default());
    let sid_raw = &data[SERVER_ID_OFFSET..SERVER_ID_OFFSET + SERVER_ID_LEN];
    let sid_end = sid_raw
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(SERVER_ID_LEN);
    let server_id = std::str::from_utf8(&sid_raw[..sid_end])
        .ok()
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .map(str::to_string);
    let name_len = data[USERNAME_OFFSET] as usize;
    let username = if name_len > 0 && name_len <= USERNAME_MAX_BYTES {
        std::str::from_utf8(&data[USERNAME_OFFSET + 1..USERNAME_OFFSET + 1 + name_len])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
    } else {
        None
    };
    TokenIdentity {
        user_id,
        server_id,
        username,
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Reference encoder/decoder for the lightyear connect token layout, used to build and
    //! check the cross-language fixture in tests/fixtures/connect_token_vector.json.
    use chacha20poly1305::aead::AeadInPlace;
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
    use std::net::SocketAddr;

    pub const VERSION: &[u8; 13] = b"NETCODE 1.02\0";
    pub const PRIVATE_BYTES: usize = 1024;
    const MAC: usize = 16;

    pub struct TokenParts {
        pub private_key: [u8; 32],
        pub protocol_id: u64,
        pub client_id: u64,
        pub create_timestamp: u64,
        pub expire_timestamp: u64,
        pub timeout_seconds: i32,
        pub nonce: [u8; 24],
        pub server_addresses: Vec<SocketAddr>,
        pub client_to_server_key: [u8; 32],
        pub server_to_client_key: [u8; 32],
        pub user_data: [u8; 256],
    }

    fn write_addresses(out: &mut Vec<u8>, addrs: &[SocketAddr]) {
        out.extend_from_slice(&(addrs.len() as u32).to_be_bytes());
        for addr in addrs {
            match addr {
                SocketAddr::V4(a) => {
                    out.push(1);
                    out.extend_from_slice(&a.ip().octets());
                    out.extend_from_slice(&a.port().to_be_bytes());
                }
                SocketAddr::V6(a) => {
                    out.push(2);
                    out.extend_from_slice(&a.ip().octets());
                    out.extend_from_slice(&a.port().to_be_bytes());
                }
            }
        }
    }

    fn aad(protocol_id: u64, expire: u64) -> Vec<u8> {
        let mut aad = VERSION.to_vec();
        aad.extend_from_slice(&protocol_id.to_be_bytes());
        aad.extend_from_slice(&expire.to_be_bytes());
        aad
    }

    pub fn private_plaintext(p: &TokenParts) -> Vec<u8> {
        let mut out = Vec::with_capacity(PRIVATE_BYTES);
        out.extend_from_slice(&p.client_id.to_be_bytes());
        out.extend_from_slice(&p.timeout_seconds.to_be_bytes());
        write_addresses(&mut out, &p.server_addresses);
        out.extend_from_slice(&p.client_to_server_key);
        out.extend_from_slice(&p.server_to_client_key);
        out.extend_from_slice(&p.user_data);
        out.resize(PRIVATE_BYTES - MAC, 0);
        out
    }

    pub fn encode(p: &TokenParts) -> Vec<u8> {
        let mut private = private_plaintext(p);
        let tag = XChaCha20Poly1305::new((&p.private_key).into())
            .encrypt_in_place_detached(
                XNonce::from_slice(&p.nonce),
                &aad(p.protocol_id, p.expire_timestamp),
                &mut private,
            )
            .expect("encrypt");
        private.extend_from_slice(tag.as_slice());

        let mut out = Vec::with_capacity(2048);
        out.extend_from_slice(VERSION);
        out.extend_from_slice(&p.protocol_id.to_be_bytes());
        out.extend_from_slice(&p.create_timestamp.to_be_bytes());
        out.extend_from_slice(&p.expire_timestamp.to_be_bytes());
        out.extend_from_slice(&p.nonce);
        out.extend_from_slice(&private);
        out.extend_from_slice(&p.timeout_seconds.to_be_bytes());
        write_addresses(&mut out, &p.server_addresses);
        out.extend_from_slice(&p.client_to_server_key);
        out.extend_from_slice(&p.server_to_client_key);
        out.resize(2048, 0);
        out
    }

    pub struct DecryptedPrivate {
        pub client_id: u64,
        pub timeout_seconds: i32,
        pub server_addresses: Vec<SocketAddr>,
        pub user_data: [u8; 256],
    }

    /// Decrypts the private part like a lightyear server would.
    pub fn decrypt_private(token: &[u8], private_key: &[u8; 32]) -> Option<DecryptedPrivate> {
        let protocol_id = u64::from_be_bytes(token[13..21].try_into().ok()?);
        let expire = u64::from_be_bytes(token[29..37].try_into().ok()?);
        let nonce = &token[37..61];
        let mut private = token[61..61 + PRIVATE_BYTES].to_vec();
        let (body, tag) = private.split_at_mut(PRIVATE_BYTES - MAC);
        XChaCha20Poly1305::new(private_key.into())
            .decrypt_in_place_detached(
                XNonce::from_slice(nonce),
                &aad(protocol_id, expire),
                body,
                chacha20poly1305::Tag::from_slice(tag),
            )
            .ok()?;
        let mut cur = 0usize;
        let mut take = |n: usize| {
            let s = &body[cur..cur + n];
            cur += n;
            s.to_vec()
        };
        let client_id = u64::from_be_bytes(take(8).try_into().ok()?);
        let timeout_seconds = i32::from_be_bytes(take(4).try_into().ok()?);
        let count = u32::from_be_bytes(take(4).try_into().ok()?);
        let mut server_addresses = Vec::new();
        for _ in 0..count {
            let kind = take(1)[0];
            let ip: std::net::IpAddr = match kind {
                1 => <[u8; 4]>::try_from(take(4)).ok()?.into(),
                2 => <[u8; 16]>::try_from(take(16)).ok()?.into(),
                _ => return None,
            };
            let port = u16::from_be_bytes(take(2).try_into().ok()?);
            server_addresses.push(SocketAddr::new(ip, port));
        }
        let _c2s = take(32);
        let _s2c = take(32);
        let user_data: [u8; 256] = take(256).try_into().ok()?;
        Some(DecryptedPrivate {
            client_id,
            timeout_seconds,
            server_addresses,
            user_data,
        })
    }

    pub fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/connect_token_vector.json"
    );

    fn vector_parts() -> TokenParts {
        let mut nonce = [0u8; 24];
        for (i, b) in nonce.iter_mut().enumerate() {
            *b = 0xa0 + i as u8;
        }
        let mut c2s = [0u8; 32];
        let mut s2c = [0u8; 32];
        for i in 0..32 {
            c2s[i] = i as u8;
            s2c[i] = 0xff - i as u8;
        }
        TokenParts {
            private_key: [0x42; 32],
            protocol_id: 0x5645_5254_4558_4941, // "VERTEXIA"
            client_id: 1234,
            create_timestamp: 1_767_225_600,
            expire_timestamp: 1_767_225_660,
            timeout_seconds: 15,
            nonce,
            server_addresses: vec!["203.0.113.10:27015".parse().unwrap()],
            client_to_server_key: c2s,
            server_to_client_key: s2c,
            user_data: encode_user_data(1234, "aB3dE5gH", None),
        }
    }

    fn fixture_json(parts: &TokenParts, token: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "description": "lightyear_netcode 0.28 connect token test vector shared by RaveEngine-Game (Rust) and VERTIGO-Website (Go). All netcode integers are BIG-endian because lightyear_serde writes them with to_be_bytes; user_data.user_id is little-endian by VERTEXIA convention.",
            "netcode_version": "NETCODE 1.02\\0",
            "integer_byte_order": "big-endian",
            "private_key_hex": hex(&parts.private_key),
            "protocol_id": parts.protocol_id.to_string(),
            "client_id": parts.client_id.to_string(),
            "create_timestamp": parts.create_timestamp,
            "expire_timestamp": parts.expire_timestamp,
            "timeout_seconds": parts.timeout_seconds,
            "nonce_hex": hex(&parts.nonce),
            "server_addresses": parts.server_addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "client_to_server_key_hex": hex(&parts.client_to_server_key),
            "server_to_client_key_hex": hex(&parts.server_to_client_key),
            "user_data": {
                "user_id": parts.client_id.to_string(),
                "user_id_encoding": "u64 little-endian at offset 0",
                "server_id": "aB3dE5gH",
                "server_id_encoding": "ASCII at offset 8, NUL padded to 32 bytes",
                "username_encoding": "optional: u8 length at offset 40, UTF-8 bytes at 41 (absent here)"
            },
            "user_data_hex": hex(&parts.user_data),
            "private_plaintext_hex": hex(&private_plaintext(parts)),
            "connect_token_base64": base64::engine::general_purpose::STANDARD.encode(token),
        })
    }

    /// Regenerate with `VERTEXIA_WRITE_TOKEN_VECTOR=1 cargo test connect_token_vector`.
    #[test]
    fn connect_token_vector_matches_fixture() {
        let parts = vector_parts();
        let token = encode(&parts);
        assert_eq!(token.len(), CONNECT_TOKEN_BYTES);
        if std::env::var("VERTEXIA_WRITE_TOKEN_VECTOR").is_ok() {
            let json = serde_json::to_string_pretty(&fixture_json(&parts, &token)).unwrap();
            std::fs::create_dir_all(std::path::Path::new(FIXTURE).parent().unwrap()).unwrap();
            std::fs::write(FIXTURE, json + "\n").unwrap();
        }

        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(FIXTURE).expect("fixture present"))
                .expect("fixture json");
        let b64 = fixture["connect_token_base64"].as_str().unwrap();
        let expected = decode_token_bytes(b64).expect("fixture token decodes");
        assert_eq!(
            expected, token,
            "reference encoder no longer matches the fixture"
        );

        let parsed = decode_connect_token(b64).expect("lightyear parses the fixture token");
        assert_eq!(
            parsed.expire_timestamp(),
            fixture["expire_timestamp"].as_u64().unwrap()
        );
        assert_eq!(
            token_expire_timestamp(&expected),
            Some(parsed.expire_timestamp())
        );

        let key: [u8; 32] = unhex(fixture["private_key_hex"].as_str().unwrap())
            .try_into()
            .unwrap();
        let private = decrypt_private(&expected, &key).expect("private part decrypts");
        let client_id: u64 = fixture["client_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(private.client_id, client_id);
        assert_eq!(private.timeout_seconds, 15);
        assert_eq!(
            private.server_addresses,
            vec!["203.0.113.10:27015".parse().unwrap()]
        );
        assert_eq!(
            hex(&private.user_data),
            fixture["user_data_hex"].as_str().unwrap()
        );
        let identity = parse_user_data(&private.user_data);
        assert_eq!(identity.user_id, client_id);
        assert_eq!(identity.server_id.as_deref(), Some("aB3dE5gH"));
        assert_eq!(identity.username, None);

        let mut wrong = key;
        wrong[0] ^= 1;
        assert!(decrypt_private(&expected, &wrong).is_none());
    }

    /// Proves the reference layout matches what lightyear itself produces.
    #[test]
    fn reference_decoder_reads_lightyear_tokens() {
        let key = [7u8; 32];
        let user_data = encode_user_data(99, "srv00001", Some("wempo"));
        let token = ConnectToken::build("198.51.100.7:4000", 77, 99, key)
            .user_data(user_data)
            .timeout_seconds(15)
            .expire_seconds(60)
            .generate()
            .unwrap();
        let bytes = token.try_into_bytes().unwrap();
        assert_eq!(&bytes[..13], VERSION);
        assert_eq!(u64::from_be_bytes(bytes[13..21].try_into().unwrap()), 77);
        let private = decrypt_private(&bytes, &key).expect("decrypts lightyear token");
        assert_eq!(private.client_id, 99);
        assert_eq!(
            private.server_addresses,
            vec!["198.51.100.7:4000".parse().unwrap()]
        );
        let identity = parse_user_data(&private.user_data);
        assert_eq!(identity.user_id, 99);
        assert_eq!(identity.server_id.as_deref(), Some("srv00001"));
        assert_eq!(identity.username.as_deref(), Some("wempo"));
    }

    #[test]
    fn rejects_bad_token_encodings() {
        assert!(matches!(
            decode_connect_token("not base64!!"),
            Err(TokenError::Base64(_))
        ));
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        assert_eq!(
            decode_connect_token(&short).err(),
            Some(TokenError::Length(10))
        );
        let zeros = base64::engine::general_purpose::STANDARD.encode([0u8; 2048]);
        assert!(matches!(
            decode_connect_token(&zeros),
            Err(TokenError::Invalid(_))
        ));
    }

    #[test]
    fn user_data_rejects_garbage_fields() {
        let mut data = [0u8; USER_DATA_BYTES];
        data[..8].copy_from_slice(&5u64.to_le_bytes());
        data[8..12].copy_from_slice(b"a/..");
        data[40] = 200;
        let id = parse_user_data(&data);
        assert_eq!(id.user_id, 5);
        assert_eq!(id.server_id, None);
        assert_eq!(id.username, None);

        let long = "é".repeat(40);
        let id = parse_user_data(&encode_user_data(1, "x", Some(&long)));
        assert_eq!(id.username.unwrap().len(), 64);
    }
}
