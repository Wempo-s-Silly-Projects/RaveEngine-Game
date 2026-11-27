use avian3d::prelude::*;
use bevy::prelude::*;
use lightyear::prelude::server::*;
use lightyear::prelude::*;
use std::time::Duration;

pub mod map;
pub mod platform;
pub mod player;

#[derive(Resource)]
pub struct ServerSettings {
    pub map_path: String,
    pub port: u16,
    pub bind_addr: std::net::IpAddr,
    pub netcode_key: [u8; 32],
    pub protocol_id: u64,
    pub allow_unauthenticated: bool,
    /// Accept HelloMessage ukeys from clients whose connect token carries no platform identity.
    pub legacy_ukey_auth: bool,
    /// Platform gameserver id; tokens sealed for a different server are rejected.
    pub server_id: Option<String>,
}

impl ServerSettings {
    pub fn hello_auth_enabled(&self) -> bool {
        self.legacy_ukey_auth || self.allow_unauthenticated
    }
}

/// uid (platform user id) of every admitted client, keyed by netcode client id.
#[derive(Resource, Default)]
pub struct AdmittedUsers(pub std::collections::HashMap<u64, i32>);

/// Set by SIGTERM/SIGINT; the server then kicks everyone, flushes leave events and exits 0.
pub static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn request_graceful_shutdown() {
    SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[derive(Resource, Default)]
pub struct ShutdownState {
    pub started_at: Option<std::time::Instant>,
    pub stopped: bool,
}

#[derive(Resource, Default)]
pub struct ClientPlayerMap(pub std::collections::HashMap<u64, Entity>);

#[derive(Clone, Copy)]
pub struct PendingAuth {
    pub entity: Entity,
    /// Identity came from the connect token; the auth worker already reported the join.
    pub via_token: bool,
    /// The client left while its join was in flight; report the leave once it resolves.
    pub disconnected: bool,
}

#[derive(Resource, Default)]
pub struct PendingAuths(pub std::collections::HashMap<u64, PendingAuth>);

const AUTH_WORKERS: usize = 16;
const AUTH_QUEUE_CAP: usize = 64;

pub enum AuthJob {
    /// Legacy HelloMessage ukey, validated against /api/v1/auth/validate.
    Ukey(String),
    /// Identity sealed in the platform connect token; reported to the platform as a join.
    Token {
        uid: i32,
        username: Option<String>,
    },
}

type AuthResult = Result<crate::common::net::auth::ValidateResponse, String>;

struct AuthWorkers {
    tx: crossbeam_channel::Sender<(u64, AuthJob)>,
    rx: std::sync::Mutex<crossbeam_channel::Receiver<(u64, AuthResult)>>,
}

fn resolve_token_identity(
    uid: i32,
    username: Option<String>,
    reporter: Option<&platform::PlatformReporterConfig>,
) -> AuthResult {
    let mut username = username;
    if let Some(reporter) = reporter {
        match reporter.report_join(uid) {
            platform::JoinReport::Accepted { username: name } => {
                if username.is_none() {
                    username = name;
                }
            }
            platform::JoinReport::Full => return Err("The server is full.".to_string()),
            platform::JoinReport::NotJoinable => {
                return Err("This game is not joinable right now.".to_string());
            }
            platform::JoinReport::Unexpected(status) => {
                warn!("platform join report for uid {uid} returned {status}; admitting anyway");
            }
            platform::JoinReport::Unreachable(e) => {
                warn!("platform join report for uid {uid} failed ({e}); admitting anyway");
            }
        }
    }
    Ok(crate::common::net::auth::ValidateResponse {
        uid,
        username: username.unwrap_or_else(|| format!("Player{uid}")),
    })
}

impl AuthWorkers {
    //pool of threads that validate ukey -- website
    fn start(
        allow_unauthenticated: bool,
        reporter: Option<platform::PlatformReporterConfig>,
    ) -> Self {
        let (job_tx, job_rx) = crossbeam_channel::bounded::<(u64, AuthJob)>(AUTH_QUEUE_CAP);
        let (res_tx, res_rx) = crossbeam_channel::bounded::<(u64, AuthResult)>(AUTH_QUEUE_CAP);
        for _ in 0..AUTH_WORKERS {
            let job_rx = job_rx.clone();
            let res_tx = res_tx.clone();
            let reporter = reporter.clone();
            std::thread::spawn(move || {
                while let Ok((client_id, job)) = job_rx.recv() {
                    let result = match job {
                        AuthJob::Ukey(ukey) => crate::common::net::auth::validate_user_ukey(
                            &ukey,
                            allow_unauthenticated,
                        ),
                        AuthJob::Token { uid, username } => {
                            resolve_token_identity(uid, username, reporter.as_ref())
                        }
                    };
                    let _ = res_tx.send((client_id, result));
                }
            });
        }
        Self {
            tx: job_tx,
            rx: std::sync::Mutex::new(res_rx),
        }
    }
}

#[derive(Resource, Default)]

//NOW we cap AuthWorker to (by default) 64 pending auth requests (woah there thats a big polytoria typa connections)
//and 16 workers. hopefully enough to handle a few hundred clients connecting??? most servers are going to be 0 - 16 players, so this should be
//wayy more than enough
pub struct AuthWorkerPool(Option<std::sync::Arc<AuthWorkers>>);

impl AuthWorkerPool {
    pub fn ensure_started(
        &mut self,
        allow_unauthenticated: bool,
        reporter: Option<&platform::PlatformReporterConfig>,
    ) {
        if self.0.is_none() {
            self.0 = Some(std::sync::Arc::new(AuthWorkers::start(
                allow_unauthenticated,
                reporter.cloned(),
            )));
        }
    }

    pub fn submit(&self, client_id: u64, job: AuthJob) -> bool {
        match &self.0 {
            Some(workers) => workers.tx.try_send((client_id, job)).is_ok(),
            None => false,
        }
    }

    pub fn drain_results(&self) -> Vec<(u64, AuthResult)> {
        let mut out = Vec::new();
        self.drain_results_into(&mut out);
        out
    }

    pub fn drain_results_into(&self, out: &mut Vec<(u64, AuthResult)>) {
        out.clear();
        if let Some(workers) = &self.0 {
            if let Ok(rx) = workers.rx.lock() {
                out.extend(rx.try_iter());
            }
        }
    }
}

pub struct ServerPlugin {
    pub map_path: String,
    pub port: u16,
    pub bind_addr: std::net::IpAddr,
    pub netcode_key: [u8; 32],
    pub protocol_id: u64,
    pub allow_unauthenticated: bool,
    pub legacy_ukey_auth: bool,
    pub server_id: Option<String>,
    pub platform: Option<platform::PlatformReporterConfig>,
}

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ServerSettings {
            map_path: self.map_path.clone(),
            port: self.port,
            bind_addr: self.bind_addr,
            netcode_key: self.netcode_key,
            protocol_id: self.protocol_id,
            allow_unauthenticated: self.allow_unauthenticated,
            legacy_ukey_auth: self.legacy_ukey_auth,
            server_id: self.server_id.clone(),
        })
        .insert_resource(Gravity(Vec3::new(0.0, -186.9 * 0.28, 0.0)))
        .insert_resource(ReplicationMetadata::new(Duration::from_secs_f64(
            1.0 / 30.0,
        )))
        .init_resource::<ClientPlayerMap>()
        .init_resource::<PendingAuths>()
        .init_resource::<AdmittedUsers>()
        .init_resource::<ShutdownState>()
        .init_resource::<AuthWorkerPool>()
        .init_resource::<player::ChatCooldowns>()
        .init_resource::<player::PlayerSoundCooldowns>()
        .add_plugins(server::ServerPlugins {
            tick_duration: Duration::from_secs_f64(1.0 / 60.0),
        })
        .add_plugins(crate::common::net::ProtocolPlugin)
        .add_systems(Startup, (setup_server, map::load_map))
        .add_systems(
            Update,
            (
                player::handle_player_moves,
                player::handle_hello_messages,
                player::handle_auth_results.after(player::handle_hello_messages),
                platform::send_heartbeats,
                player::graceful_shutdown,
                player::sync_players_service_properties,
                player::handle_chat_messages,
                player::handle_sound_requests,
                crate::common::game::assets::sounds::advance_sound_positions,
                crate::common::game::assets::sounds::reset_sound_positions_on_change,
            ),
        )
        .add_systems(
            FixedPostUpdate,
            player::apply_player_movement
                .in_set(PhysicsSystems::Prepare)
                .after(avian3d::physics_transform::PhysicsTransformSystems::TransformToPosition),
        )
        .add_systems(PostUpdate, player::sync_transforms_to_network)
        .add_observer(player::handle_new_client)
        .add_observer(player::handle_client_disconnect)
        .add_observer(announce_ready);
        if let Some(config) = self.platform.clone() {
            info!(
                "Platform reporting enabled for gameserver {} via {}",
                config.server_id, config.origin
            );
            app.insert_resource(platform::PlatformReporter::start(config));
        } else {
            info!("Platform reporting disabled (no server id or GAMESERVER_API_KEY)");
        }
    }
}

/// Printed once the UDP socket is bound; the platform agent waits for this line.
fn announce_ready(
    trigger: On<Add, Started>,
    servers: Query<(), With<NetcodeServer>>,
    settings: Res<ServerSettings>,
) {
    if servers.get(trigger.entity).is_err() {
        return;
    }
    let line = format!(
        "VERTEXIA_SERVER_READY bind={}:{} server_id={}",
        settings.bind_addr,
        settings.port,
        settings.server_id.as_deref().unwrap_or("-")
    );
    info!("{line}");
    println!("{line}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn setup_server(mut commands: Commands, settings: Res<ServerSettings>) {
    info!(
        "Starting setup_server system on {}:{}",
        settings.bind_addr, settings.port
    );
    let bind_addr = std::net::SocketAddr::new(settings.bind_addr, settings.port);

    if settings.netcode_key == [0u8; 32] {
        warn!("NETCODE_PRIVATE_KEY is all zeros: anyone can mint connect tokens (dev only)");
    }
    if settings.legacy_ukey_auth {
        warn!("Legacy HelloMessage ukey authentication is enabled");
    }
    if settings.allow_unauthenticated {
        warn!("studio keys allowed -- playtets/stress test so um if this is unintended fuck!");
    }

    let netcode_config = NetcodeConfig {
        protocol_id: settings.protocol_id,
        private_key: settings.netcode_key,
        client_timeout_secs: 15,
        ..Default::default()
    };

    let server_entity = commands
        .spawn((
            Server::default(),
            ServerUdpIo::default(),
            LocalAddr(bind_addr),
            NetcodeServer::new(netcode_config),
            Transport::new(PriorityConfig::default()).with_compression(CompressionConfig::LZ4),
        ))
        .id();

    commands.trigger(Start {
        entity: server_entity,
    });
    info!("Server entity spawned and Start trigger dispatched");

    commands.insert_resource(crate::scripting::vm::server_vm::ServerScriptVM::new());

    //spawn the services to replicate to client
    commands.spawn((Name::new("Workspace"),));

    commands.spawn((
        Name::new("Players"),
        crate::common::net::components::PlayersServiceContainer, //Players, contains defs for speed, jump power and some other player specific attributes
        Replicate::default(),
    ));

    commands.spawn((
        Name::new("Lighting"),
        crate::common::net::components::LightingServiceContainer, //lighting, contains time of day, ambient light etc
        Replicate::default(),
    ));

    commands.spawn((
        Name::new("AssetService"),
        crate::common::net::components::AssetServiceContainer, //for assets, images and in the future meshes and sounds
        Replicate::default(),
    ));
}
