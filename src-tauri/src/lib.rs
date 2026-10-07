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
    LatestAlerts, LatestMetrics, TokenEpochState, WakeState, add_server, copy_to_clipboard,
    delete_grafana_token, get_config, get_latest_alerts, get_latest_metrics, has_grafana_token,
    inspect_kubeconfig, open_pod_logs, open_ssh_terminal, open_url, pick_kubeconfig, pick_ssh_key,
    quit_app, remove_server, resize_window, save_settings, set_grafana_token,
};
use config::ConfigStore;
use grafana::TokenEpoch;
use poller::{Poller, PollerHandles};
use tray::setup_tray_and_window;

/// Run the Observer Ward application.
///
/// # Errors
///
/// Returns an error if the config directory cannot be determined, the
/// Tauri runtime fails to start, or the tray icon cannot be created.
#[expect(
    clippy::exit,
    reason = "tauri::generate_context! macro calls process::exit"
)]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Record the opt-out before AppKit enables automatic-termination
    // support for the hidden popover. See `disable_automatic_termination`.
    #[cfg(target_os = "macos")]
    tray::disable_automatic_termination();

    let config_path = config::config_path()?;
    let initial_config = config::load_config_or_default(&config_path);
    let config = Arc::new(ConfigStore::new(config_path, initial_config));
    let is_window_visible = Arc::new(AtomicBool::new(false));
    let poll_wake = Arc::new(Notify::new());
    let token_epoch = Arc::new(TokenEpoch::default());
    let latest_metrics = Arc::new(Mutex::new(None));
    let latest_alerts = Arc::new(Mutex::new(None));

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .manage(Arc::clone(&config))
        .manage(WakeState(Arc::clone(&poll_wake)))
        .manage(TokenEpochState(Arc::clone(&token_epoch)))
        .manage(LatestMetrics(Arc::clone(&latest_metrics)))
        .manage(LatestAlerts(Arc::clone(&latest_alerts)))
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_settings,
            add_server,
            pick_kubeconfig,
            pick_ssh_key,
            inspect_kubeconfig,
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

            let mut poller = Poller::new(PollerHandles {
                app_handle: app.handle().clone(),
                config,
                is_visible: is_window_visible,
                wake: poll_wake,
                token_epoch,
                latest_metrics,
                latest_alerts,
            });
            tauri::async_runtime::spawn(async move { poller.run().await });

            Ok(())
        })
        .run(tauri::generate_context!())?;

    Ok(())
}
