use crate::app::client::config::ClientAppConfig;
use crate::app::common::log::setup_app_logging;
use crate::client::ClientPlugin;
use crate::client::session::ClientSessionPlugin;
use crate::common::CommonPlugin;
use bevy::prelude::*;
use lightyear::prelude::client::ClientPlugins;
use std::time::Duration;

pub struct RaveClientApp {
    config: ClientAppConfig,
}

impl RaveClientApp {
    pub fn new(config: ClientAppConfig) -> Self {
        Self { config }
    }

    pub fn run(self) {
        let log_plugin = setup_app_logging("client");

        let mut app = App::new();
        app.add_plugins(
            DefaultPlugins
                .set(log_plugin)
                .set(crate::app::common::assets::asset_plugin())
                .set(bevy::render::RenderPlugin {
                    render_creation: bevy::render::settings::RenderCreation::Automatic(Box::new(
                        bevy::render::settings::WgpuSettings {
                            disabled_features: Some(
                                bevy::render::settings::WgpuFeatures::TEXTURE_BINDING_ARRAY,
                            ),
                            ..default()
                        },
                    )),
                    ..default()
                }),
        );
        app.add_plugins(ClientPlugins {
            tick_duration: Duration::from_secs_f64(1.0 / 60.0),
        });
        app.add_plugins(CommonPlugin);
        app.add_plugins(ClientPlugin);
        app.add_plugins(ClientSessionPlugin {
            config: self.config.launch,
        });
        app.run();
    }
}
