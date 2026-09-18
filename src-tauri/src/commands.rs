//! Tauri IPC commands invoked by the frontend.

use std::sync::{Arc, Mutex};

use tauri::State;
use tokio::sync::Notify;

use crate::config;
use crate::error;
use crate::grafana;
use crate::metrics;
use crate::ssh;
use crate::terminal::{run_in_terminal, validate_shell_safe};

pub(crate) struct ConfigState(pub(crate) Arc<Mutex<config::AppConfig>>);
pub(crate) struct WakeState(pub(crate) Arc<Notify>);
pub(crate) struct LatestMetrics(pub(crate) Arc<Mutex<Option<metrics::MetricsUpdate>>>);
pub(crate) struct LatestAlerts(pub(crate) Arc<Mutex<Option<metrics::AlertsUpdate>>>);

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
pub(crate) fn get_config(state: State<'_, ConfigState>) -> Result<config::AppConfig, String> {
    let config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(config.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State \
              and deserialized parameters"
)]
pub(crate) fn save_config_cmd(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    new_config: config::AppConfig,
) -> Result<(), String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    config::save_config(&new_config).map_err(|e| error::error_chain(&e))?;
    *config = new_config;
    drop(config);
    wake.0.notify_one();
    Ok(())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State \
              and deserialized parameters"
)]
pub(crate) fn add_server(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    server: config::ServerConfig,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    if config.servers.iter().any(|s| s.name() == server.name()) {
        return Err(format!("server '{}' already exists", server.name()));
    }
    let mut next = config.clone();
    next.servers.push(server);
    config::save_config(&next).map_err(|e| error::error_chain(&e))?;
    *config = next.clone();
    drop(config);
    wake.0.notify_one();
    Ok(next)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State \
              and deserialized parameters"
)]
pub(crate) fn remove_server(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    name: String,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    let mut next = config.clone();
    let before = next.servers.len();
    next.servers.retain(|s| s.name() != name);
    if next.servers.len() == before {
        return Err(format!("server '{name}' not found"));
    }
    config::save_config(&next).map_err(|e| error::error_chain(&e))?;
    *config = next.clone();
    drop(config);
    wake.0.notify_one();
    Ok(next)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro injects WebviewWindow by value"
)]
pub(crate) fn resize_window(
    window: tauri::WebviewWindow,
    width: f64,
    height: f64,
) -> Result<(), String> {
    window
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|e| format!("resize failed: {e}"))
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn open_ssh_terminal(
    host: String,
    port: u16,
    user: String,
    key_path: String,
) -> Result<(), String> {
    let host = ssh::ssh_cli_host(&host);
    let key_path = config::expand_tilde(&key_path);
    validate_shell_safe(&host, "host")?;
    validate_shell_safe(&user, "user")?;
    validate_shell_safe(&key_path, "key_path")?;
    let cmd = format!("ssh '{user}'@'{host}' -p {port} -i '{key_path}'");
    run_in_terminal(&cmd)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn open_pod_logs(
    pod_name: String,
    namespace: String,
    context: String,
    kubeconfig: Option<String>,
) -> Result<(), String> {
    validate_shell_safe(&pod_name, "pod_name")?;
    validate_shell_safe(&namespace, "namespace")?;
    validate_shell_safe(&context, "context")?;
    let kubeconfig = kubeconfig.map(|kc| config::expand_tilde(&kc));
    if let Some(kc) = &kubeconfig {
        validate_shell_safe(kc, "kubeconfig")?;
    }
    let cmd = if let Some(kc) = &kubeconfig {
        format!(
            "kubectl logs -f '{pod_name}' -n '{namespace}' \
             --context '{context}' --kubeconfig '{kc}'"
        )
    } else {
        format!(
            "kubectl logs -f '{pod_name}' -n '{namespace}' \
             --context '{context}'"
        )
    };
    run_in_terminal(&cmd)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned AppHandle"
)]
pub(crate) fn copy_to_clipboard(app: tauri::AppHandle, text: String) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard()
        .write_text(&text)
        .map_err(|e| format!("clipboard write failed: {e}"))
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn set_grafana_token(
    wake: State<'_, WakeState>,
    name: String,
    token: String,
) -> Result<(), String> {
    let entry = keyring::Entry::new(grafana::KEYCHAIN_SERVICE, &name)
        .map_err(|e| format!("keychain error: {e}"))?;
    entry
        .set_password(&token)
        .map_err(|e| format!("keychain write failed: {e}"))?;
    wake.0.notify_one();
    Ok(())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn has_grafana_token(name: String) -> bool {
    // Returns false on any error (missing token or keychain failure); the
    // UI only needs "is it configured", and never reads the secret back.
    grafana::read_token(&name).is_ok()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn delete_grafana_token(wake: State<'_, WakeState>, name: String) -> Result<(), String> {
    let entry = keyring::Entry::new(grafana::KEYCHAIN_SERVICE, &name)
        .map_err(|e| format!("keychain error: {e}"))?;
    match entry.delete_credential() {
        // Deleting a token that was never stored is a no-op success.
        Ok(()) | Err(keyring::Error::NoEntry) => {
            wake.0.notify_one();
            Ok(())
        }
        Err(e) => Err(format!("keychain delete failed: {e}")),
    }
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
pub(crate) fn get_latest_metrics(
    state: State<'_, LatestMetrics>,
) -> Result<Option<metrics::MetricsUpdate>, String> {
    let guard = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(guard.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
pub(crate) fn get_latest_alerts(
    state: State<'_, LatestAlerts>,
) -> Result<Option<metrics::AlertsUpdate>, String> {
    let guard = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(guard.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn open_url(url: String) -> Result<(), String> {
    // http(s) only — refuse file://, custom schemes, or app launches. The
    // URL is passed to `open` as a single argv entry (no shell), so
    // query-string characters cannot be interpreted as shell syntax; scheme
    // validation is the only check needed. Scheme is case-insensitive per
    // RFC 3986, so lowercase a copy for the guard while passing the original
    // url (path/query case preserved) to `open`.
    let scheme_ok = {
        let lower = url.to_ascii_lowercase();
        lower.starts_with("https://") || lower.starts_with("http://")
    };
    if !scheme_ok {
        return Err("url must be http(s)".to_string());
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("failed to open url: {e}"))?;
    Ok(())
}
