//! End to end: a client holding a platform-style connect token joins a production-mode server
//! (no hello login) and is admitted with the identity sealed in the token. A token minted for
//! another server is kicked.

use RaveEngineLib::common::net::connect_token::{decode_connect_token, encode_user_data};
use RaveEngineLib::common::net::messages::{AuthSuccessMessage, KickMessage};
use base64::Engine;
use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use lightyear::netcode::ConnectToken;
use lightyear::prelude::client::*;
use lightyear::prelude::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

const KEY: [u8; 32] = [0x42; 32];
const PROTOCOL_ID: u64 = 0x5645_5254_4558_4941;
const SERVER_ID: &str = "aB3dE5gH";

fn free_port() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("failed to bind probe socket");
    socket.local_addr().unwrap().port()
}

fn build_server_app(port: u16) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(16))));
    app.add_plugins(RaveEngineLib::common::assets_path::asset_plugin());
    app.init_asset::<bevy::render::mesh::Mesh>();
    app.add_plugins(StatesPlugin);
    app.add_plugins(TransformPlugin);
    app.add_plugins(RaveEngineLib::common::CommonPlugin);
    app.add_plugins(RaveEngineLib::server::ServerPlugin {
        map_path: "assets/maps/does_not_exist.vrtx".to_string(),
        port,
        bind_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        netcode_key: KEY,
        protocol_id: PROTOCOL_ID,
        allow_unauthenticated: false,
        legacy_ukey_auth: false,
        server_id: Some(SERVER_ID.to_string()),
        platform: None,
    });
    app.finish();
    app
}

#[derive(Resource, Default)]
struct Outcome {
    admitted: Option<(i32, String)>,
    kicked: Option<String>,
}

fn record_messages(
    mut success: Query<&mut MessageReceiver<AuthSuccessMessage>>,
    mut kicks: Query<&mut MessageReceiver<KickMessage>>,
    mut outcome: ResMut<Outcome>,
) {
    for mut receiver in &mut success {
        for msg in receiver.receive() {
            outcome.admitted = Some((msg.uid, msg.username));
        }
    }
    for mut receiver in &mut kicks {
        for msg in receiver.receive() {
            outcome.kicked = Some(msg.reason);
        }
    }
}

fn connect_after_warmup(
    mut commands: Commands,
    mut frames: Local<u32>,
    clients: Query<Entity, With<Client>>,
) {
    *frames += 1;
    if *frames == 3 {
        for entity in &clients {
            commands.trigger(Connect { entity });
        }
    }
}

/// Mints a token the way the platform does and passes it through the client's decoder.
fn platform_token(port: u16, user_id: u64, server_id: &str) -> ConnectToken {
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let bytes = ConnectToken::build(server_addr, PROTOCOL_ID, user_id, KEY)
        .timeout_seconds(15)
        .expire_seconds(60)
        .user_data(encode_user_data(user_id, server_id, Some("wempo")))
        .generate()
        .unwrap()
        .try_into_bytes()
        .unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    decode_connect_token(&b64).unwrap()
}

fn build_client_app(token: ConnectToken) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(16))));
    app.add_plugins(StatesPlugin);
    app.add_plugins(TransformPlugin);
    app.add_plugins(ClientPlugins {
        tick_duration: Duration::from_secs_f64(1.0 / 60.0),
    });
    app.add_plugins(RaveEngineLib::common::net::ProtocolPlugin);
    app.init_resource::<Outcome>();
    // Same components as the native client's session (src/client/session.rs).
    app.world_mut().spawn((
        Client::default(),
        UdpIo::default(),
        NetcodeClient::new(
            Authentication::Token(token),
            client::NetcodeConfig {
                client_timeout_secs: 15,
                ..default()
            },
        )
        .unwrap(),
        LocalAddr(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)),
    ));
    app.add_systems(Update, (connect_after_warmup, record_messages));
    app.finish();
    app
}

fn run_until(server: &mut App, client: &mut App, done: impl Fn(&Outcome) -> bool) {
    for _ in 0..1200 {
        server.update();
        client.update();
        if done(client.world().resource::<Outcome>()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn platform_token_is_admitted_without_hello() {
    let port = free_port();
    let mut server = build_server_app(port);
    let mut client = build_client_app(platform_token(port, 4242, SERVER_ID));
    run_until(&mut server, &mut client, |o| {
        o.admitted.is_some() || o.kicked.is_some()
    });
    let outcome = client.world().resource::<Outcome>();
    assert_eq!(outcome.kicked, None);
    assert_eq!(outcome.admitted, Some((4242, "wempo".to_string())));
}

#[test]
fn token_for_another_server_is_kicked() {
    let port = free_port();
    let mut server = build_server_app(port);
    let mut client = build_client_app(platform_token(port, 4243, "zzzzzzzz"));
    run_until(&mut server, &mut client, |o| {
        o.admitted.is_some() || o.kicked.is_some()
    });
    let outcome = client.world().resource::<Outcome>();
    assert_eq!(outcome.admitted, None);
    assert!(outcome.kicked.is_some(), "expected a kick");
}
