use crate::client::launch::ClientLaunchConfig;

pub struct ClientAppConfig {
    pub launch: ClientLaunchConfig,
}

impl ClientAppConfig {
    pub fn from_env_and_args() -> Result<Self, String> {
        crate::client::launch::from_env_and_args().map(|launch| Self { launch })
    }
}
