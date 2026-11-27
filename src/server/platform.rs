//! Reports gameserver join/leave/heartbeat events to the VERTEXIA platform.
//!
//! All HTTP runs on a dedicated thread; the Bevy main thread only enqueues events.
//! `GAMESERVER_API_KEY` is sent exclusively in the `X-Gameserver-Key` header.

use bevy::prelude::*;
use std::time::{Duration, Instant};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct PlatformReporterConfig {
    pub origin: String,
    pub server_id: String,
    pub api_key: String,
}

impl std::fmt::Debug for PlatformReporterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformReporterConfig")
            .field("origin", &self.origin)
            .field("server_id", &self.server_id)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl PlatformReporterConfig {
    /// Reporting is enabled when the server knows its platform id and has an API key.
    pub fn from_env(server_id: Option<&str>) -> Option<Self> {
        let server_id = server_id?.trim();
        if server_id.is_empty() {
            return None;
        }
        let api_key = crate::common::net::api::gameserver_api_key().ok()?;
        if api_key.is_empty() {
            return None;
        }
        Some(Self {
            origin: crate::common::net::api::api_base(),
            server_id: server_id.to_string(),
            api_key,
        })
    }

    fn url(&self, action: &str) -> String {
        format!(
            "{}/api/v1/gameserver/servers/{}/{}",
            self.origin, self.server_id, action
        )
    }

    fn client(&self) -> Result<reqwest::blocking::Client, String> {
        crate::common::net::api::blocking_client(REQUEST_TIMEOUT)
    }

    fn post(
        &self,
        client: &reqwest::blocking::Client,
        action: &str,
        body: serde_json::Value,
    ) -> Result<(u16, String), String> {
        let resp = client
            .post(self.url(action))
            .header("X-Gameserver-Key", &self.api_key)
            .json(&body)
            .send()
            .map_err(|e| e.without_url().to_string())?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        Ok((status, text))
    }

    /// Synchronous join report used by the auth workers (never the main thread).
    pub fn report_join(&self, uid: i32) -> JoinReport {
        let client = match self.client() {
            Ok(c) => c,
            Err(e) => return JoinReport::Unreachable(e),
        };
        match self.post(&client, "join", serde_json::json!({ "uid": uid })) {
            Ok((status, body)) => classify_join(status, &body),
            Err(e) => JoinReport::Unreachable(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinReport {
    Accepted {
        username: Option<String>,
    },
    Full,
    NotJoinable,
    /// The platform answered with something unexpected; the player is still admitted because
    /// the connect token already proves who they are.
    Unexpected(u16),
    Unreachable(String),
}

pub fn classify_join(status: u16, body: &str) -> JoinReport {
    match status {
        200..=299 => {
            let username = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("username")
                        .or_else(|| v.get("user").and_then(|u| u.get("username")))
                        .and_then(|u| u.as_str())
                        .map(str::to_string)
                })
                .filter(|u| !u.trim().is_empty());
            JoinReport::Accepted { username }
        }
        409 => JoinReport::Full,
        403 => JoinReport::NotJoinable,
        other => JoinReport::Unexpected(other),
    }
}

enum ReportEvent {
    Leave { uid: i32, player_count: usize },
    Heartbeat { player_count: usize },
    Join { uid: i32, player_count: usize },
}

/// Background reporter for fire-and-forget events.
#[derive(Resource)]
pub struct PlatformReporter {
    config: PlatformReporterConfig,
    tx: Option<crossbeam_channel::Sender<ReportEvent>>,
    done: crossbeam_channel::Receiver<()>,
    last_heartbeat: Option<Instant>,
}

impl PlatformReporter {
    pub fn start(config: PlatformReporterConfig) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<ReportEvent>();
        let (done_tx, done) = crossbeam_channel::bounded::<()>(1);
        let worker_config = config.clone();
        std::thread::Builder::new()
            .name("platform-reporter".to_string())
            .spawn(move || {
                let client = match worker_config.client() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("platform reporter disabled: {e}");
                        let _ = done_tx.send(());
                        return;
                    }
                };
                while let Ok(event) = rx.recv() {
                    let (action, body) = match event {
                        ReportEvent::Leave { uid, player_count } => (
                            "leave",
                            serde_json::json!({ "uid": uid, "player_count": player_count }),
                        ),
                        ReportEvent::Join { uid, player_count } => (
                            "join",
                            serde_json::json!({ "uid": uid, "player_count": player_count }),
                        ),
                        ReportEvent::Heartbeat { player_count } => (
                            "heartbeat",
                            serde_json::json!({ "player_count": player_count }),
                        ),
                    };
                    match worker_config.post(&client, action, body) {
                        Ok((status, _)) if (200..300).contains(&status) => {
                            debug!("platform {action} reported");
                        }
                        Ok((status, _)) => warn!("platform {action} returned {status}"),
                        Err(e) => warn!("platform {action} failed: {e}"),
                    }
                }
                let _ = done_tx.send(());
            })
            .expect("failed to spawn platform reporter");
        Self {
            config,
            tx: Some(tx),
            done,
            last_heartbeat: None,
        }
    }

    pub fn config(&self) -> &PlatformReporterConfig {
        &self.config
    }

    fn send(&self, event: ReportEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    pub fn join(&self, uid: i32, player_count: usize) {
        self.send(ReportEvent::Join { uid, player_count });
    }

    pub fn leave(&self, uid: i32, player_count: usize) {
        self.send(ReportEvent::Leave { uid, player_count });
    }

    pub fn heartbeat(&mut self, player_count: usize) {
        self.last_heartbeat = Some(Instant::now());
        self.send(ReportEvent::Heartbeat { player_count });
    }

    pub fn heartbeat_due(&self, now: Instant) -> bool {
        self.last_heartbeat
            .is_none_or(|last| now.duration_since(last) >= HEARTBEAT_INTERVAL)
    }

    /// Stops accepting events and waits (bounded) for queued ones to be delivered.
    pub fn flush(&mut self, timeout: Duration) -> bool {
        self.tx.take();
        self.done.recv_timeout(timeout).is_ok()
    }
}

pub fn send_heartbeats(
    reporter: Option<ResMut<PlatformReporter>>,
    players: Res<crate::server::ClientPlayerMap>,
    shutdown: Res<crate::server::ShutdownState>,
) {
    let Some(mut reporter) = reporter else {
        return;
    };
    if shutdown.started_at.is_some() {
        return;
    }
    if reporter.heartbeat_due(Instant::now()) {
        reporter.heartbeat(players.0.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_reports_are_classified() {
        assert_eq!(
            classify_join(200, r#"{"success":true}"#),
            JoinReport::Accepted { username: None }
        );
        assert_eq!(
            classify_join(200, r#"{"username":"wempo"}"#),
            JoinReport::Accepted {
                username: Some("wempo".into())
            }
        );
        assert_eq!(classify_join(409, ""), JoinReport::Full);
        assert_eq!(classify_join(403, ""), JoinReport::NotJoinable);
        assert_eq!(classify_join(500, ""), JoinReport::Unexpected(500));
    }

    #[test]
    fn api_key_is_never_in_the_url_or_debug_output() {
        let config = PlatformReporterConfig {
            origin: "https://example.test".into(),
            server_id: "aB3dE5gH".into(),
            api_key: "super-secret".into(),
        };
        let url = config.url("heartbeat");
        assert_eq!(
            url,
            "https://example.test/api/v1/gameserver/servers/aB3dE5gH/heartbeat"
        );
        assert!(!url.contains("super-secret"));
        assert!(!format!("{config:?}").contains("super-secret"));
    }
}
