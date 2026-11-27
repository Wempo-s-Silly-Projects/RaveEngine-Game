//! Native client session: browser login, game browser, platform join and the UDP connection.
//!
//! The platform's `/games/:id/join` returns a netcode connect token sealed with the game
//! server's private key, so the client never needs that key. Failures leave the player on a
//! screen with a reason and a Retry button; retrying always fetches a fresh token.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use lightyear::netcode::ConnectToken;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

use crate::client::ServerSessionEvents;
use crate::client::launch::{ClientLaunchConfig, LaunchTarget, LegacyConnect};
use crate::common::net::api::{
    ApiError, ApiTask, GameList, LoginAudience, PlatformClient, TaskPoll,
};
use crate::common::net::connect_token::decode_connect_token;
use crate::common::net::messages::SHUTDOWN_KICK_REASON;
use crate::studio::auth::{self, StudioAuthFlow, StudioAuthStore};

/// Frames between spawning the client and connecting: lets the renderer warm up and makes our
/// `PeerAddr` land after the one `NetcodeClient`'s insert hook copies from the token.
const CONNECT_WARMUP_FRAMES: u32 = 30;
/// Backstop in case netcode never reports a result.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Server-side timeout for tokens we build ourselves in dev legacy mode.
const CLIENT_TIMEOUT_SECS: i32 = 15;

pub struct ClientSessionPlugin {
    pub config: ClientLaunchConfig,
}

impl Plugin for ClientSessionPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClientSession::new(&self.config))
            .insert_resource(StudioAuthFlow::for_audience(LoginAudience::Client))
            .init_resource::<StudioAuthStore>()
            .add_systems(
                Startup,
                start_session.after(crate::client::setup_player_assets),
            )
            .add_systems(Update, drive_session)
            .add_systems(bevy_egui::EguiPrimaryContextPass, draw_session_ui);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionPhase {
    NeedLogin,
    LoggingIn,
    Browsing,
    Joining,
    Connecting,
    Playing,
    Failed { message: String, retryable: bool },
}

#[derive(Debug, Clone, PartialEq)]
enum SessionAction {
    Login,
    CancelLogin,
    Logout,
    Search,
    Page(u32),
    Join(u64),
    Retry,
    Cancel,
    BackToGames,
    Quit,
}

/// Everything a join needs, prepared off the main thread.
struct PreparedJoin {
    token: ConnectToken,
    addr: SocketAddr,
    server_id: String,
}

struct PendingConnection {
    entity: Entity,
    addr: SocketAddr,
    frames: u32,
    connect_sent: bool,
    handshake_started: bool,
    started: Instant,
}

#[derive(Resource)]
pub struct ClientSession {
    pub phase: SessionPhase,
    /// Game to join after login, and the one Retry rejoins.
    pub game_id: Option<u64>,
    pub notice: Option<String>,
    login_error: Option<String>,
    legacy: Option<LegacyConnect>,
    search: String,
    page: u32,
    games: Option<GameList>,
    games_error: Option<String>,
    games_task: Option<ApiTask<Result<GameList, ApiError>>>,
    join_task: Option<ApiTask<Result<PreparedJoin, ApiError>>>,
    connection: Option<PendingConnection>,
    action: Option<SessionAction>,
}

impl ClientSession {
    fn new(config: &ClientLaunchConfig) -> Self {
        let (game_id, legacy) = match &config.target {
            LaunchTarget::Browse => (None, None),
            LaunchTarget::Game(id) => (Some(*id), None),
            LaunchTarget::LegacyDirect(legacy) => (None, Some(legacy.clone())),
        };
        Self {
            phase: SessionPhase::NeedLogin,
            game_id,
            notice: config.notice.clone(),
            login_error: None,
            legacy,
            search: String::new(),
            page: 1,
            games: None,
            games_error: None,
            games_task: None,
            join_task: None,
            connection: None,
            action: None,
        }
    }

    fn fail(&mut self, message: impl Into<String>, retryable: bool) {
        self.join_task = None;
        self.phase = SessionPhase::Failed {
            message: message.into(),
            retryable,
        };
    }

    fn fetch_games(&mut self) {
        let (page, query) = (self.page, self.search.clone());
        self.games_error = None;
        self.games_task = Some(ApiTask::spawn(move || {
            PlatformClient::new()?.games(page, &query)
        }));
    }

    fn browse(&mut self) {
        self.phase = SessionPhase::Browsing;
        self.fetch_games();
    }

    fn start_join(&mut self, game_id: u64, token: String) {
        self.game_id = Some(game_id);
        self.phase = SessionPhase::Joining;
        self.join_task = Some(ApiTask::spawn(move || prepare_join(game_id, &token)));
    }

    /// Continues after a successful login: join the requested game or show the browser.
    fn logged_in(&mut self, token: &str) {
        match self.game_id {
            Some(id) => self.start_join(id, token.to_string()),
            None => self.browse(),
        }
    }
}

/// Calls `/join`, decodes the token and resolves the server address. Runs on a worker thread.
fn prepare_join(game_id: u64, access_token: &str) -> Result<PreparedJoin, ApiError> {
    let join = PlatformClient::new()?.join(game_id, access_token)?;
    let token =
        decode_connect_token(&join.connect_token).map_err(|e| ApiError::Decode(e.to_string()))?;
    let addr = resolve_server(&join.host, join.port)?;
    Ok(PreparedJoin {
        token,
        addr,
        server_id: join.server_id,
    })
}

fn resolve_server(host: &str, port: u16) -> Result<SocketAddr, ApiError> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| ApiError::Network(format!("could not resolve game server {host}: {e}")))?
        .collect();
    // Prefer IPv4: it is what the dedicated servers are known to listen on.
    addrs.sort_by_key(|a| a.is_ipv6());
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::Network(format!("game server {host} has no address")))
}

fn unspecified_for(addr: SocketAddr) -> SocketAddr {
    let ip = match addr {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    SocketAddr::new(ip, 0)
}

fn spawn_client(
    commands: &mut Commands,
    auth: Authentication,
    addr: SocketAddr,
) -> Result<PendingConnection, String> {
    let netcode = NetcodeClient::new(
        auth,
        NetcodeConfig {
            client_timeout_secs: CLIENT_TIMEOUT_SECS,
            ..default()
        },
    )
    .map_err(|e| format!("could not start the network client: {e:?}"))?;
    let entity = commands
        .spawn((
            Client::default(),
            UdpIo::default(),
            netcode,
            LocalAddr(unspecified_for(addr)),
        ))
        .id();
    Ok(PendingConnection {
        entity,
        addr,
        frames: 0,
        connect_sent: false,
        handshake_started: false,
        started: Instant::now(),
    })
}

/// Player-facing text for a lightyear disconnect reason and an optional kick reason.
pub fn describe_disconnect(reason: Option<&str>, kick: Option<&str>) -> String {
    if let Some(kick) = kick {
        if kick == SHUTDOWN_KICK_REASON {
            return "The server is shutting down. Join again to move to another server."
                .to_string();
        }
        return format!("You were removed from the server: {kick}");
    }
    let reason = reason.unwrap_or_default();
    if reason.contains("ConnectTokenExpired") {
        "Your join ticket expired before the server answered. Try again.".to_string()
    } else if reason.contains("ConnectionDenied") {
        "The game server refused the connection. It may be full.".to_string()
    } else if reason.contains("ConnectionRequestTimedOut")
        || reason.contains("ChallengeResponseTimedOut")
    {
        "Could not reach the game server. Check your connection or firewall (UDP) and try again."
            .to_string()
    } else if reason.contains("ConnectionTimedOut") {
        "Lost connection to the game server.".to_string()
    } else {
        "Disconnected from the game server.".to_string()
    }
}

/// Dev only: connects with a token built from the shared key and sends the legacy ukey hello.
fn connect_legacy(commands: &mut Commands, session: &mut ClientSession) {
    let Some(legacy) = session.legacy.clone() else {
        return;
    };
    warn!(
        "Dev legacy connect to {}: building a connect token locally",
        legacy.addr
    );
    commands.insert_resource(crate::client::ClientUkey(legacy.ukey.clone()));
    let client_id = rand::random::<u64>();
    commands.insert_resource(crate::client::LocalClientId(client_id));
    let auth = Authentication::Manual {
        server_addr: legacy.addr,
        client_id,
        private_key: legacy.netcode_key,
        protocol_id: legacy.protocol_id,
    };
    match spawn_client(commands, auth, legacy.addr) {
        Ok(conn) => {
            session.connection = Some(conn);
            session.phase = SessionPhase::Connecting;
        }
        Err(e) => session.fail(e, false),
    }
}

fn start_session(
    mut commands: Commands,
    mut session: ResMut<ClientSession>,
    mut store: ResMut<StudioAuthStore>,
    mut flow: ResMut<StudioAuthFlow>,
) {
    if session.legacy.is_some() {
        connect_legacy(&mut commands, &mut session);
        return;
    }

    let creds = auth::load_credentials(LoginAudience::Client).filter(|c| c.bearer().is_some());
    match creds {
        Some(creds) => {
            flow.validate_in_background(&creds);
            let token = creds.access_token.clone();
            info!("Logged in to VERTEXIA as {}", creds.username);
            store.credentials = Some(creds);
            session.logged_in(&token);
        }
        None => session.phase = SessionPhase::NeedLogin,
    }
}

fn current_token(store: &StudioAuthStore) -> Option<String> {
    store
        .credentials
        .as_ref()
        .filter(|c| !c.is_expired_at(auth::unix_now()))
        .and_then(|c| c.bearer())
        .map(str::to_string)
}

fn require_login(
    session: &mut ClientSession,
    store: &mut StudioAuthStore,
    flow: &mut StudioAuthFlow,
) {
    auth::forget_credentials(LoginAudience::Client);
    store.credentials = None;
    flow.reset();
    session.join_task = None;
    session.games_task = None;
    session.login_error = Some(ApiError::Unauthorized.to_string());
    session.phase = SessionPhase::NeedLogin;
}

fn drop_connection(commands: &mut Commands, session: &mut ClientSession) {
    if let Some(conn) = session.connection.take() {
        commands.trigger(Disconnect {
            entity: conn.entity,
        });
        commands.entity(conn.entity).try_despawn();
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_session(
    mut commands: Commands,
    mut session: ResMut<ClientSession>,
    mut store: ResMut<StudioAuthStore>,
    mut flow: ResMut<StudioAuthFlow>,
    mut events: ResMut<ServerSessionEvents>,
    clients: Query<(Has<Connecting>, Has<Connected>, Option<&Disconnected>), With<Client>>,
    mut exit: MessageWriter<AppExit>,
) {
    let session = &mut *session;
    let store = &mut *store;
    let flow = &mut *flow;

    if let Some(action) = session.action.take() {
        match action {
            SessionAction::Login => {
                session.login_error = None;
                match auth::start_auth_flow(flow, LoginAudience::Client) {
                    Ok(()) => session.phase = SessionPhase::LoggingIn,
                    Err(e) => session.login_error = Some(e),
                }
            }
            SessionAction::CancelLogin => {
                flow.reset();
                session.phase = SessionPhase::NeedLogin;
            }
            SessionAction::Logout => {
                drop_connection(&mut commands, session);
                auth::logout(LoginAudience::Client, store.credentials.take().as_ref());
                flow.reset();
                session.join_task = None;
                session.games = None;
                session.game_id = None;
                session.phase = SessionPhase::NeedLogin;
            }
            SessionAction::Search => {
                session.page = 1;
                session.fetch_games();
            }
            SessionAction::Page(page) => {
                session.page = page.max(1);
                session.fetch_games();
            }
            SessionAction::Join(game_id) => match current_token(store) {
                Some(token) => {
                    session.notice = None;
                    session.start_join(game_id, token);
                }
                None => {
                    session.game_id = Some(game_id);
                    require_login(session, store, flow);
                }
            },
            SessionAction::Retry => {
                drop_connection(&mut commands, session);
                *events = ServerSessionEvents::default();
                if session.legacy.is_some() {
                    connect_legacy(&mut commands, session);
                    return;
                }
                match (session.game_id, current_token(store)) {
                    (Some(id), Some(token)) => session.start_join(id, token),
                    (Some(_), None) => require_login(session, store, flow),
                    (None, _) => session.browse(),
                }
            }
            SessionAction::Cancel => {
                drop_connection(&mut commands, session);
                session.join_task = None;
                session.browse();
            }
            SessionAction::BackToGames => {
                drop_connection(&mut commands, session);
                session.game_id = None;
                session.browse();
            }
            SessionAction::Quit => {
                exit.write(AppExit::Success);
            }
        }
    }

    // Browser login.
    if session.phase == SessionPhase::LoggingIn {
        let result = flow.receiver.as_ref().and_then(|rx| rx.try_recv().ok());
        match result {
            Some(Ok(creds)) if creds.bearer().is_some() => {
                if let Err(e) = auth::save_credentials(LoginAudience::Client, &creds) {
                    warn!("{e}");
                }
                let token = creds.access_token.clone();
                info!("Logged in to VERTEXIA as {}", creds.username);
                store.credentials = Some(creds);
                flow.reset();
                session.logged_in(&token);
            }
            Some(Ok(_)) => {
                flow.reset();
                session.login_error = Some("The platform did not issue an access token.".into());
                session.phase = SessionPhase::NeedLogin;
            }
            Some(Err(e)) => {
                flow.reset();
                session.login_error = Some(e);
                session.phase = SessionPhase::NeedLogin;
            }
            None if flow
                .waiting_since
                .is_some_and(|t| t.elapsed() > LOGIN_TIMEOUT) =>
            {
                flow.reset();
                session.login_error = Some("Login timed out. Try again.".into());
                session.phase = SessionPhase::NeedLogin;
            }
            None => {}
        }
    }

    // Background check of a stored token.
    if let Some(validation) = flow.validation.as_ref() {
        match validation.poll() {
            TaskPoll::Ready(Err(ApiError::Unauthorized)) => {
                warn!("Stored VERTEXIA session was rejected; please log in again");
                if session.phase != SessionPhase::Playing {
                    drop_connection(&mut commands, session);
                    require_login(session, store, flow);
                } else {
                    // Already in a game: keep playing, ask for a login next time.
                    auth::forget_credentials(LoginAudience::Client);
                    store.credentials = None;
                    flow.validation = None;
                }
            }
            TaskPoll::Ready(Err(e)) => {
                debug!("Could not verify the stored session: {e}");
                flow.validation = None;
            }
            TaskPoll::Ready(Ok(())) | TaskPoll::Abandoned => flow.validation = None,
            TaskPoll::Pending => {}
        }
    }

    // Game list.
    if let Some(task) = session.games_task.as_ref() {
        match task.poll() {
            TaskPoll::Ready(Ok(list)) => {
                session.games = Some(list);
                session.games_task = None;
            }
            TaskPoll::Ready(Err(e)) => {
                session.games_error = Some(e.to_string());
                session.games_task = None;
            }
            TaskPoll::Abandoned => {
                session.games_error = Some("Loading games failed.".into());
                session.games_task = None;
            }
            TaskPoll::Pending => {}
        }
    }

    // Join request.
    if let Some(task) = session.join_task.as_ref() {
        match task.poll() {
            TaskPoll::Ready(Ok(prepared)) => {
                session.join_task = None;
                *events = ServerSessionEvents::default();
                info!(
                    "Joining game {:?} on server {} at {}",
                    session.game_id, prepared.server_id, prepared.addr
                );
                match spawn_client(
                    &mut commands,
                    Authentication::Token(prepared.token),
                    prepared.addr,
                ) {
                    Ok(conn) => {
                        session.connection = Some(conn);
                        session.phase = SessionPhase::Connecting;
                    }
                    Err(e) => session.fail(e, true),
                }
            }
            TaskPoll::Ready(Err(ApiError::Unauthorized)) => require_login(session, store, flow),
            TaskPoll::Ready(Err(e)) => {
                let retryable = e.is_retryable();
                session.fail(e.to_string(), retryable);
            }
            TaskPoll::Abandoned => session.fail("Joining failed unexpectedly.", true),
            TaskPoll::Pending => {}
        }
    }

    // UDP connection.
    let Some(conn) = session.connection.as_mut() else {
        return;
    };
    conn.frames += 1;
    if !conn.connect_sent {
        if conn.frames >= CONNECT_WARMUP_FRAMES {
            commands.entity(conn.entity).insert(PeerAddr(conn.addr));
            commands.trigger(Connect {
                entity: conn.entity,
            });
            conn.connect_sent = true;
            conn.started = Instant::now();
            info!("Connecting to game server at {}", conn.addr);
        }
        return;
    }
    let Ok((connecting, connected, disconnected)) = clients.get(conn.entity) else {
        session.connection = None;
        session.fail("The network client stopped unexpectedly.", true);
        return;
    };
    if connecting || connected {
        conn.handshake_started = true;
    }
    if let Some(disconnected) = disconnected.filter(|_| conn.handshake_started) {
        let message = describe_disconnect(
            disconnected.reason.as_deref(),
            events.kick_reason.as_deref(),
        );
        info!("Disconnected: {:?}", disconnected.reason);
        let entity = conn.entity;
        session.connection = None;
        commands.entity(entity).try_despawn();
        session.fail(message, true);
        return;
    }
    match session.phase {
        SessionPhase::Connecting if connected && events.admitted => {
            session.phase = SessionPhase::Playing;
        }
        SessionPhase::Connecting if conn.started.elapsed() > HANDSHAKE_TIMEOUT => {
            drop_connection(&mut commands, session);
            session.fail(
                "The game server did not answer. Check your connection and try again.",
                true,
            );
        }
        _ => {}
    }
}

fn draw_session_ui(
    mut contexts: EguiContexts,
    mut session: ResMut<ClientSession>,
    store: Res<StudioAuthStore>,
    flow: Res<StudioAuthFlow>,
) {
    if session.phase == SessionPhase::Playing {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let session = &mut *session;
    let mut action = None;
    egui::Window::new("VERTEXIA")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .min_width(420.0)
        .show(ctx, |ui| {
            if let Some(notice) = &session.notice {
                ui.label(notice);
                ui.separator();
            }
            match session.phase.clone() {
                SessionPhase::NeedLogin => {
                    ui.label("Log in to VERTEXIA to play.");
                    if let Some(err) = &session.login_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, err);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Log in with browser").clicked() {
                            action = Some(SessionAction::Login);
                        }
                        if ui.button("Quit").clicked() {
                            action = Some(SessionAction::Quit);
                        }
                    });
                }
                SessionPhase::LoggingIn => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Finish logging in in your browser.");
                    });
                    if let Some(err) = &flow.error {
                        ui.colored_label(egui::Color32::LIGHT_RED, err);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(SessionAction::CancelLogin);
                    }
                }
                SessionPhase::Browsing => draw_browser(ui, session, &store, &mut action),
                SessionPhase::Joining => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Finding a game server...");
                    });
                    if ui.button("Cancel").clicked() {
                        action = Some(SessionAction::Cancel);
                    }
                }
                SessionPhase::Connecting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Connecting to the game server...");
                    });
                    if session.legacy.is_none() && ui.button("Cancel").clicked() {
                        action = Some(SessionAction::Cancel);
                    }
                }
                SessionPhase::Failed { message, retryable } => {
                    ui.colored_label(egui::Color32::LIGHT_RED, message);
                    ui.horizontal(|ui| {
                        if retryable && ui.button("Try again").clicked() {
                            action = Some(SessionAction::Retry);
                        }
                        if session.legacy.is_none() && ui.button("Back to games").clicked() {
                            action = Some(SessionAction::BackToGames);
                        }
                        if ui.button("Quit").clicked() {
                            action = Some(SessionAction::Quit);
                        }
                    });
                }
                SessionPhase::Playing => {}
            }
        });
    if action.is_some() {
        session.action = action;
    }
}

fn draw_browser(
    ui: &mut egui::Ui,
    session: &mut ClientSession,
    store: &StudioAuthStore,
    action: &mut Option<SessionAction>,
) {
    ui.horizontal(|ui| {
        if let Some(creds) = &store.credentials {
            ui.label(format!("Logged in as {}", creds.username));
        }
        if ui.button("Log out").clicked() {
            *action = Some(SessionAction::Logout);
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        let search = ui.text_edit_singleline(&mut session.search);
        let enter = search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Search").clicked() || enter {
            *action = Some(SessionAction::Search);
        }
    });
    if session.games_task.is_some() {
        ui.spinner();
    }
    if let Some(err) = &session.games_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
        if ui.button("Reload").clicked() {
            *action = Some(SessionAction::Page(session.page));
        }
    }
    if let Some(list) = &session.games {
        if list.games.is_empty() {
            ui.label("No games found.");
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for game in &list.games {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.strong(&game.name);
                            let players = if game.max_players > 0 {
                                format!("{}/{} playing", game.players_online, game.max_players)
                            } else {
                                format!("{} playing", game.players_online)
                            };
                            ui.small(players);
                            if !game.description.is_empty() {
                                let short: String = game.description.chars().take(120).collect();
                                ui.small(short);
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Join").clicked() {
                                *action = Some(SessionAction::Join(game.id));
                            }
                        });
                    });
                    ui.separator();
                }
            });
        ui.horizontal(|ui| {
            if session.page > 1 && ui.button("Previous").clicked() {
                *action = Some(SessionAction::Page(session.page - 1));
            }
            ui.label(format!("Page {}", session.page));
            if let Some(next) = list.next_page {
                if ui.button("Next").clicked() {
                    *action = Some(SessionAction::Page(next));
                }
            }
        });
    }
    if ui.button("Quit").clicked() {
        *action = Some(SessionAction::Quit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_kicks_and_netcode_failures() {
        assert!(describe_disconnect(None, Some(SHUTDOWN_KICK_REASON)).contains("shutting down"));
        assert_eq!(
            describe_disconnect(Some("Client trigger"), Some("Spamming")),
            "You were removed from the server: Spamming"
        );
        assert!(
            describe_disconnect(Some("Client disconnected: ConnectTokenExpired"), None)
                .contains("expired")
        );
        assert!(
            describe_disconnect(Some("Client disconnected: ConnectionDenied"), None)
                .contains("refused")
        );
        assert!(
            describe_disconnect(Some("Client disconnected: ConnectionRequestTimedOut"), None)
                .contains("Could not reach")
        );
        assert!(
            describe_disconnect(Some("Client disconnected: ChallengeResponseTimedOut"), None)
                .contains("Could not reach")
        );
        assert!(
            describe_disconnect(Some("Client disconnected: ConnectionTimedOut"), None)
                .contains("Lost connection")
        );
        assert_eq!(
            describe_disconnect(None, None),
            "Disconnected from the game server."
        );
    }

    #[test]
    fn resolves_literal_and_bracketed_addresses() {
        assert_eq!(
            resolve_server("203.0.113.10", 27015).unwrap(),
            "203.0.113.10:27015".parse().unwrap()
        );
        assert_eq!(
            resolve_server("[::1]", 5000).unwrap(),
            "[::1]:5000".parse().unwrap()
        );
        assert_eq!(resolve_server("localhost", 5000).unwrap().port(), 5000);
    }

    #[test]
    fn binds_the_matching_address_family() {
        assert!(unspecified_for("1.2.3.4:5".parse().unwrap()).is_ipv4());
        assert!(unspecified_for("[::1]:5".parse().unwrap()).is_ipv6());
    }

    #[test]
    fn launch_target_seeds_the_session() {
        let config = |target| ClientLaunchConfig {
            target,
            no_clouds: false,
            notice: None,
        };
        let session = ClientSession::new(&config(LaunchTarget::Game(9)));
        assert_eq!(session.game_id, Some(9));
        assert!(session.legacy.is_none());
        let session = ClientSession::new(&config(LaunchTarget::Browse));
        assert_eq!(session.game_id, None);
        assert_eq!(session.phase, SessionPhase::NeedLogin);
    }
}
