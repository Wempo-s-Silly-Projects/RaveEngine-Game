#[cfg(target_os = "android")] //Hey android, die!!
fn main() {}

#[cfg(not(target_os = "android"))]
mod desktop_studio {
    use RaveEngineLib::common::CommonPlugin;
    use RaveEngineLib::studio::StudioPlugin;
    use bevy::log::LogPlugin;
    use bevy::prelude::*;

    pub fn main() {
        for arg in std::env::args().skip(1) {
            let lower = arg.to_lowercase();
            if lower == "--no-clouds" || lower == "--disable-clouds" {
                unsafe {
                    std::env::set_var("VERTIGO_NO_CLOUDS", "1");
                }
            }
            if lower.starts_with("vertexia-studio://") || lower.starts_with("vertexia://") {
                if let Some(q) = arg.split('?').nth(1) {
                    let mut code_opt: Option<String> = None;
                    for pair in q.split('&') {
                        let mut kv = pair.splitn(2, '=');
                        if kv.next() == Some("code") {
                            code_opt = kv.next().map(|v| v.to_string());
                        }
                    }
                    if let Some(code) = code_opt {
                        if !code.is_empty() {
                            // Runs before the Bevy app exists, so a blocking exchange is fine here.
                            match RaveEngineLib::studio::auth::exchange_code(&code) {
                                Ok(creds) => {
                                    if let Err(e) =
                                        RaveEngineLib::studio::auth::save_studio_credentials(&creds)
                                    {
                                        eprintln!("{e}");
                                        std::process::exit(1);
                                    }
                                    println!(
                                        "VERTEXIA Studio authenticated as {} (uid {}) via URI",
                                        creds.username, creds.uid
                                    );
                                    std::process::exit(0);
                                }
                                Err(e) => {
                                    eprintln!("Failed to exchange studio auth code via URI: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
                }
            }
        }
        let rust_log = std::env::var("RUST_LOG").unwrap_or_default();
        let new_rust_log = if rust_log.is_empty() {
            "info,wgpu=warn,naga=warn,wgpu_hal=warn,wgpu_core=warn,offset_allocator=off".to_string()
        } else if !rust_log.contains("offset_allocator") {
            format!("{rust_log},offset_allocator=off")
        } else {
            rust_log
        };
        unsafe {
            std::env::set_var("VERTIGO_APP", "studio");
            std::env::set_var("RUST_LOG", new_rust_log);
        }
        App::new()
            .insert_resource(bevy_egui::EguiGlobalSettings {
                auto_create_primary_context: false,
                ..default()
            })
            .add_plugins(DefaultPlugins.set(LogPlugin {
                filter: "info,wgpu=warn,naga=warn,wgpu_hal=warn,wgpu_core=warn,offset_allocator=off".to_string(),
                ..default()
            }).set(RaveEngineLib::common::assets_path::asset_plugin()).set(bevy::render::RenderPlugin {
                render_creation: bevy::render::settings::RenderCreation::Automatic(Box::new(
                    bevy::render::settings::WgpuSettings {
                        disabled_features: Some(bevy::render::settings::WgpuFeatures::TEXTURE_BINDING_ARRAY),
                        ..default()
                    }
                )),
                ..default()
            }))
            .add_plugins(lightyear::prelude::client::ClientPlugins {
                tick_duration: core::time::Duration::from_secs_f64(1.0 / 60.0),
            })
            .add_plugins(CommonPlugin)
            .add_plugins(RaveEngineLib::client::ClientPlugin)
            .add_plugins(StudioPlugin)
            .run();
    }
}

#[cfg(not(target_os = "android"))]
fn main() {
    desktop_studio::main();
}
