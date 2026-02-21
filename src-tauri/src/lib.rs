mod config;
mod metrics;
#[expect(dead_code, reason = "used by later tasks (poll loop, frontend)")]
mod ssh_backend;
#[expect(dead_code, reason = "used by later tasks (poll loop, frontend)")]
mod k8s_backend;

use std::sync::Mutex;
use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, State,
};
use tauri_plugin_positioner::{Position, WindowExt};

struct ConfigState(Mutex<config::AppConfig>);

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
fn get_config(state: State<'_, ConfigState>) -> Result<config::AppConfig, String> {
    let config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(config.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State and deserialized parameters"
)]
fn save_config_cmd(
    state: State<'_, ConfigState>,
    new_config: config::AppConfig,
) -> Result<(), String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    config::save_config(&new_config)?;
    *config = new_config;
    Ok(())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State and deserialized parameters"
)]
fn add_server(
    state: State<'_, ConfigState>,
    server: config::ServerConfig,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    if config.servers.iter().any(|s| s.name() == server.name()) {
        return Err(format!("server '{}' already exists", server.name()));
    }
    config.servers.push(server);
    config::save_config(&config)?;
    Ok(config.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State and deserialized parameters"
)]
fn remove_server(state: State<'_, ConfigState>, name: String) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    config.servers.retain(|s| s.name() != name);
    config::save_config(&config)?;
    Ok(config.clone())
}

/// Run the Observer Ward application.
///
/// # Errors
///
/// Returns an error if the Tauri runtime fails to start, the tray
/// icon cannot be created, or the default window icon is missing.
#[expect(
    clippy::exit,
    reason = "tauri::generate_context! macro calls process::exit"
)]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let initial_config = config::load_config().unwrap_or_default();

    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .manage(ConfigState(Mutex::new(initial_config)))
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_cmd,
            add_server,
            remove_server,
        ])
        .setup(|app| {
            let icon = app
                .default_window_icon()
                .ok_or("no default icon configured")?
                .clone();

            let _tray = TrayIconBuilder::new()
                .icon(icon)
                .icon_as_template(true)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                if let Err(e) = window.hide() {
                                    tracing::warn!("failed to hide window: {e}");
                                }
                            } else {
                                if let Err(e) = window.move_window(Position::TrayCenter) {
                                    tracing::warn!("failed to position window: {e}");
                                }
                                if let Err(e) = window.show() {
                                    tracing::warn!("failed to show window: {e}");
                                }
                                if let Err(e) = window.set_focus() {
                                    tracing::warn!("failed to focus window: {e}");
                                }
                            }
                        }
                    }
                })
                .build(app)?;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            Ok(())
        })
        .run(tauri::generate_context!())?;

    Ok(())
}
