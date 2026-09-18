//! Observer Ward — tray dashboard for Kubernetes, SSH hosts, and Grafana alerts.

mod commands;
mod config;
mod error;
mod grafana;
mod k8s;
mod metrics;
mod poller;
mod ssh;
mod terminal;
mod tray;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tauri_plugin_autostart::MacosLauncher;
use tokio::sync::Notify;

use commands::{
    ConfigState, LatestAlerts, LatestMetrics, WakeState, add_server, copy_to_clipboard,
    delete_grafana_token, get_config, get_latest_alerts, get_latest_metrics, has_grafana_token,
    open_pod_logs, open_ssh_terminal, open_url, quit_app, remove_server, resize_window,
    save_config_cmd, set_grafana_token,
};
use poller::PollerHandles;
use tray::setup_tray_and_window;

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

    // keyring v4 requires a credential store to be registered before any
    // Entry operation; register the platform-native store (macOS Keychain)
    // once at startup. A failure here only disables Grafana token storage —
    // the rest of the app still works — so log and continue.
    if let Err(e) = keyring::use_native_store(false) {
        tracing::warn!("failed to initialize keychain store: {e}");
    }

    let initial_config = match config::load_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(
                "failed to load config, using defaults: {}",
                error::error_chain(&e)
            );
            config::AppConfig::default()
        }
    };
    let config_arc = Arc::new(Mutex::new(initial_config));
    let is_window_visible = Arc::new(AtomicBool::new(false));
    let poll_wake = Arc::new(Notify::new());
    let latest_metrics = Arc::new(Mutex::new(None));
    let latest_alerts = Arc::new(Mutex::new(None));

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .manage(ConfigState(Arc::clone(&config_arc)))
        .manage(WakeState(Arc::clone(&poll_wake)))
        .manage(LatestMetrics(Arc::clone(&latest_metrics)))
        .manage(LatestAlerts(Arc::clone(&latest_alerts)))
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_cmd,
            add_server,
            remove_server,
            resize_window,
            open_ssh_terminal,
            open_pod_logs,
            copy_to_clipboard,
            set_grafana_token,
            has_grafana_token,
            delete_grafana_token,
            get_latest_metrics,
            get_latest_alerts,
            open_url,
            quit_app,
        ])
        .setup(move |app| {
            setup_tray_and_window(app, &is_window_visible, &poll_wake)?;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let config_for_poller = Arc::clone(&config_arc);
            tauri::async_runtime::spawn(async move {
                let mut poller = poller::Poller::new(PollerHandles {
                    app_handle: handle,
                    config_state: config_for_poller,
                    is_visible: is_window_visible,
                    wake: poll_wake,
                    latest_metrics,
                    latest_alerts,
                });
                poller.run().await;
            });

            Ok(())
        })
        .run(tauri::generate_context!())?;

    Ok(())
}
