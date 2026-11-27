use RaveEngineLib::client::ClientPlugin;
use RaveEngineLib::client::session::ClientSessionPlugin;
use RaveEngineLib::common::CommonPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use lightyear::prelude::client::*;

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

    let launch = match RaveEngineLib::client::launch::from_env_and_args() {
        Ok(launch) => launch,
        Err(e) => {
            eprintln!("RaveEngineClient: {e}");
            std::process::exit(2);
        }
    };
    if launch.no_clouds {
        unsafe {
            std::env::set_var("VERTIGO_NO_CLOUDS", "1");
        }
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
    app.add_plugins(ClientPlugins {
        tick_duration: core::time::Duration::from_secs_f64(1.0 / 60.0),
    });
    app.add_plugins(CommonPlugin);
    app.add_plugins(ClientPlugin);
    app.add_plugins(ClientSessionPlugin { config: launch });
    app.run();
}
