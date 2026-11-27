use RaveEngineLib::client::ClientPlugin;
use RaveEngineLib::common::CommonPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

#[derive(Resource)]
struct ClientConnectSettings {
    ip: std::net::IpAddr,
    port: u16,
    connect_token: Option<Vec<u8>>,
    netcode_key: Option<[u8; 32]>,
    protocol_id: Option<u64>,
}

#[bevy_main]
fn main() {
    let rust_log = std::env::var("RUST_LOG").unwrap_or_default();
    let new_rust_log = if rust_log.is_empty() {
        "debug,wgpu=error,bevy_render=error,bevy_ecs=warn,lightyear=debug,lightyear_udp=trace,lightyear_netcode=trace,naga=warn,wgpu_hal=warn,wgpu_core=warn,offset_allocator=off".to_string()
    } else if !rust_log.contains("offset_allocator") {
        format!("{rust_log},offset_allocator=off")
    } else {
        rust_log
    };
    unsafe {
        std::env::set_var("VERTIGO_APP", "client");
        std::env::set_var("RUST_LOG", new_rust_log);
    }
    let _ = RaveEngineLib::client::uri::register_client_uri_scheme();

    let mut ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
    let mut port = 5000;
    let mut ukey = std::env::var("VERTEXIA_CLIENT_TOKEN")
        .or_else(|_| std::env::var("VERTIGO_CLIENT_UKEY"))
        .unwrap_or_default();
    let mut connect_token: Option<Vec<u8>> = None;
    let mut api_origin = RaveEngineLib::common::net::api::api_base();
    let mut launch_code = String::new();
    let mut netcode_key_cli: Option<String> = None;
    let mut protocol_id_cli: Option<u64> = None;

    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if let Some(join) = RaveEngineLib::client::uri::parse_client_join_uri(&args[i]) {
            ip = join.ip;
            port = join.port;
            if !join.ukey.is_empty() { ukey = join.ukey; }
            if !join.connect_token.is_empty() { connect_token = Some(RaveEngineLib::common::net::api::decode_connect_token(&join.connect_token).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1); })); }
            if !join.code.is_empty() { launch_code = join.code; }
            if !join.api_base.is_empty() { api_origin = join.api_base; }
        }
        if args[i] == "--no-clouds" || args[i] == "--disable-clouds" {
            unsafe { std::env::set_var("VERTIGO_NO_CLOUDS", "1"); }
        }
        if args[i] == "--port" && i + 1 < args.len() {
            if let Ok(p) = args[i + 1].parse::<u16>() { port = p; }
        }
        if args[i] == "--ip" && i + 1 < args.len() {
            if let Ok(ip_addr) = args[i + 1].parse::<std::net::IpAddr>() { ip = ip_addr; }
        }
        if args[i] == "--ukey" && i + 1 < args.len() { ukey = args[i + 1].clone(); }
        if args[i] == "--netcode-key" && i + 1 < args.len() { netcode_key_cli = Some(args[i + 1].clone()); }
        if args[i] == "--protocol-id" && i + 1 < args.len() {
            if let Ok(id) = args[i + 1].parse::<u64>() { protocol_id_cli = Some(id); }
        }
    }

    if !launch_code.is_empty() {
        unsafe { std::env::set_var("VERTEXIA_API_DOMAIN", &api_origin); }
        ukey = RaveEngineLib::common::net::api::exchange_client_code(&api_origin, &launch_code)
            .unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1); });
    }
    if ukey.is_empty() {
        eprintln!("No VERTEXIA client authorization token was provided. Launch the game from the VERTEXIA website.");
        std::process::exit(1);
    }

    let netcode_key = netcode_key_cli
        .as_deref()
        .and_then(RaveEngineLib::common::net::netcode::parse_hex_key)
        .or_else(|| std::env::var("NETCODE_PRIVATE_KEY").ok().as_deref().and_then(RaveEngineLib::common::net::netcode::parse_hex_key));
    let protocol_id = protocol_id_cli.or_else(|| std::env::var("NETCODE_PROTOCOL_ID").ok().and_then(|v| v.trim().parse::<u64>().ok()));
    if connect_token.is_none() && (netcode_key.is_none() || protocol_id.is_none()) {
        eprintln!("No connect token or complete explicit development Netcode configuration was provided.");
        std::process::exit(1);
    }

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(LogPlugin {
        level: bevy::log::Level::DEBUG,
        filter: "wgpu=error,bevy_render=error,bevy_ecs=warn,lightyear=debug,lightyear_udp=trace,lightyear_netcode=trace,naga=warn,wgpu_hal=warn,wgpu_core=warn,offset_allocator=off".to_string(),
        ..default()
    }).set(RaveEngineLib::common::assets_path::asset_plugin()).set(bevy::render::RenderPlugin {
        render_creation: bevy::render::settings::RenderCreation::Automatic(Box::new(
            bevy::render::settings::WgpuSettings {
                disabled_features: Some(bevy::render::settings::WgpuFeatures::TEXTURE_BINDING_ARRAY),
                ..default()
            }
        )),
        ..default()
    }));
    app.insert_resource(ClientConnectSettings {
        ip,
        port,
        connect_token,
        netcode_key,
        protocol_id,
    });
    app.insert_resource(RaveEngineLib::client::ClientUkey(ukey));
    app.add_plugins(client::ClientPlugins {
        tick_duration: core::time::Duration::from_secs_f64(1.0 / 60.0),
    });
    app.add_plugins(CommonPlugin);
    app.add_plugins(ClientPlugin);
    app.add_systems(
        Startup,
        setup_client.after(RaveEngineLib::client::setup_player_assets),
    );
    app.add_systems(Update, trigger_delayed_connect);
    app.run();
}

fn setup_client(mut commands: Commands, settings: Res<ClientConnectSettings>) {
    let server_addr = std::net::SocketAddr::new(settings.ip, settings.port);
    let client_id = rand::random::<u64>();

    commands.insert_resource(RaveEngineLib::client::LocalClientId(client_id));

    let auth = if let Some(bytes) = settings.connect_token.as_deref() {
        match lightyear::netcode::ConnectToken::try_from_bytes(bytes) {
            Ok(token) => Authentication::Token(token),
            Err(err) => {
                error!("Invalid VERTEXIA connect token: {err}");
                return;
            }
        }
    } else if let (Some(private_key), Some(protocol_id)) = (settings.netcode_key, settings.protocol_id) {
        warn!("Using manual Netcode authentication. This is for local development only.");
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
        LocalAddr(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            0,
        )),
        PeerAddr(server_addr),
    ));
}

fn trigger_delayed_connect(
    mut commands: Commands,
    mut frame_count: Local<u32>,
    client_query: Query<Entity, With<Client>>,
    mut connected: Local<bool>,
) {
    if *connected {
        return;
    }
    *frame_count += 1;
    if *frame_count >= 30 {
        for entity in &client_query {
            commands.trigger(Connect { entity });
            info!("Handshake connection triggered after rendering warmup");
        }
        *connected = true;
    }
}
