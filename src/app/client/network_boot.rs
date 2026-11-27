use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::*;
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Resource)]
pub struct ClientConnectSettings {
    pub ip: IpAddr,
    pub port: u16,
    pub connect_token: Option<Vec<u8>>,
    pub netcode_key: Option<[u8; 32]>,
    pub protocol_id: Option<u64>,
}

#[derive(Resource, Default)]
pub struct ClientSpawned(pub bool);

#[derive(Deserialize)]
pub struct LaunchInfo {
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub ukey: String,
    #[serde(default)]
    pub connect_token: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub netcode_key: Option<String>,
    #[serde(default)]
    pub protocol_id: Option<u64>,
}

pub fn poll_launch_details(
    mut commands: Commands,
    ukey_res: Option<Res<crate::client::ClientUkey>>,
    mut settings: Option<ResMut<ClientConnectSettings>>,
    mut frame_counter: Local<u32>,
) {
    if let Some(ukey) = &ukey_res {
        if !ukey.0.is_empty() {
            return;
        }
    }

    *frame_counter += 1;
    if *frame_counter % 10 != 0 {
        return;
    }

    let launch_path = crate::common::assets_path::resolve_launch_info_path();
    if let Ok(file_content) = std::fs::read_to_string(&launch_path)
        .or_else(|_| std::fs::read_to_string("launch_info.json"))
    {
        if let Ok(info) = serde_json::from_str::<LaunchInfo>(&file_content) {
            if let Ok(parsed_ip) = info.ip.parse::<IpAddr>() {
                let connect_token = match info.connect_token.as_deref() {
                    Some(value) => match crate::common::net::api::decode_connect_token(value) {
                        Ok(token) => Some(token),
                        Err(err) => {
                            error!("Invalid VERTEXIA connect token in launch_info.json: {err}");
                            return;
                        }
                    },
                    None => None,
                };
                let netcode_key = info.netcode_key.as_deref()
                    .and_then(crate::common::net::netcode::parse_hex_key);
                let mut access_token = info.ukey;
                if let Some(code) = info.code.as_deref().filter(|value| !value.is_empty()) {
                    let api = info.api.as_deref().filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(crate::common::net::api::api_base);
                    unsafe { std::env::set_var("VERTEXIA_API_DOMAIN", &api); }
                    access_token = match crate::common::net::api::exchange_client_code(&api, code) {
                        Ok(token) => token,
                        Err(err) => {
                            error!("VERTEXIA client authorization exchange failed: {err}");
                            return;
                        }
                    };
                }
                commands.insert_resource(crate::client::ClientUkey(access_token));
                if let Some(ref mut s) = settings {
                    s.ip = parsed_ip;
                    s.port = info.port;
                    s.connect_token = connect_token;
                    s.netcode_key = netcode_key;
                    s.protocol_id = info.protocol_id;
                } else {
                    commands.insert_resource(ClientConnectSettings {
                        ip: parsed_ip,
                        port: info.port,
                        connect_token,
                        netcode_key,
                        protocol_id: info.protocol_id,
                    });
                }
                info!(
                    "CLIENT_CONNECT: Successfully loaded connection details from launch_info.json"
                );
                let _ = std::fs::remove_file(&launch_path);
                let _ = std::fs::remove_file("launch_info.json");
            }
        }
    }
}

pub fn initialize_client(
    mut commands: Commands,
    settings: Res<ClientConnectSettings>,
    ukey_res: Option<Res<crate::client::ClientUkey>>,
    mut spawned: ResMut<ClientSpawned>,
) {
    if spawned.0 {
        return;
    }
    let Some(ukey) = ukey_res else {
        return;
    };
    if ukey.0.is_empty() {
        return;
    }

    let server_addr = SocketAddr::new(settings.ip, settings.port);
    let client_id = rand::random::<u64>();

    commands.insert_resource(crate::client::LocalClientId(client_id));

    let netcode_key_env = std::env::var("NETCODE_PRIVATE_KEY").ok();
    let private_key = settings.netcode_key.or_else(|| netcode_key_env.as_deref()
        .and_then(crate::common::net::netcode::parse_hex_key));
    let protocol_id = settings.protocol_id.or_else(|| std::env::var("NETCODE_PROTOCOL_ID")
        .ok().and_then(|value| value.parse::<u64>().ok()));
    let auth = if let Some(bytes) = settings.connect_token.as_deref() {
        match lightyear::netcode::ConnectToken::try_from_bytes(bytes) {
            Ok(token) => Authentication::Token(token),
            Err(err) => { error!("Invalid VERTEXIA connect token: {err}"); return; }
        }
    } else if let (Some(private_key), Some(protocol_id)) = (private_key, protocol_id) {
        warn!("Using manual Netcode authentication for local development.");
        Authentication::Manual { server_addr, client_id, private_key, protocol_id }
    } else {
        error!("No valid connect token was provided.");
        return;
    };

    let netcode_config = NetcodeConfig {
        client_timeout_secs: 15,
        ..default()
    };

    commands.spawn((
        Client::default(),
        UdpIo::default(),
        NetcodeClient::new(auth, netcode_config).unwrap(),
        LocalAddr(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)),
        PeerAddr(server_addr),
        Transport::new(PriorityConfig::default()).with_compression(CompressionConfig::LZ4),
    ));

    spawned.0 = true;
    info!(
        "CLIENT_CONNECT: Spawned Client entity with server_addr: {}, client_id: {}",
        server_addr, client_id
    );
}

pub fn trigger_delayed_connect(
    mut commands: Commands,
    mut frame_count: Local<u32>,
    client_query: Query<Entity, With<Client>>,
    mut connected: Local<bool>,
    ukey_res: Option<Res<crate::client::ClientUkey>>,
) {
    if *connected {
        return;
    }
    let Some(ukey) = ukey_res else {
        trace!("CLIENT_CONNECT: Waiting for ClientUkey resource to be inserted...");
        return;
    };
    if ukey.0.is_empty() {
        trace!("CLIENT_CONNECT: Waiting for valid ClientUkey details...");
        return;
    }
    *frame_count += 1;
    if *frame_count >= 30 {
        for entity in &client_query {
            commands.trigger(Connect { entity });
            info!(
                "CLIENT_CONNECT: Handshake connection triggered after rendering warmup with authenticated credentials"
            );
        }
        *connected = true;
    }
}
