//! Tauri IPC commands invoked by the frontend.

use std::sync::{Arc, Mutex};

use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Notify;

use crate::config;
use crate::error;
use crate::grafana;
use crate::k8s;
use crate::metrics;
use crate::ssh;
use crate::terminal::{run_in_terminal, validate_shell_safe};
use crate::tray::NativeDialogGuard;

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
pub(crate) async fn add_server(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    server: config::ServerConfig,
) -> Result<config::AppConfig, String> {
    // Runs before the config lock is taken: no std::sync guard across await.
    validate_new_server(&server).await?;

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

/// Reject entries the poller could never connect with (a mistyped
/// kubeconfig path or context) instead of saving a server that only ever
/// shows "offline". SSH entries are checked by connecting, not here.
async fn validate_new_server(server: &config::ServerConfig) -> Result<(), String> {
    match server {
        config::ServerConfig::K8s {
            kubeconfig,
            context,
            ..
        } => k8s::validate_server(kubeconfig.clone(), context.clone())
            .await
            .map_err(|e| error::error_chain(&e)),
        config::ServerConfig::Ssh { .. } => Ok(()),
    }
}

/// Show a native file picker for a kubeconfig and return the chosen path,
/// or `None` if the user cancelled. `current` is the path already in the
/// form; the picker opens in its folder, else in `~/.kube`.
#[tauri::command]
pub(crate) async fn pick_kubeconfig(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    current: Option<String>,
) -> Result<Option<String>, String> {
    let fallback = dirs::home_dir().map(|home| home.join(".kube"));
    let start_dir = config::picker_start_dir(current.as_deref(), fallback.as_deref());
    pick_file_path(&app, &window, "Select kubeconfig", start_dir).await
}

/// Like [`pick_kubeconfig`], for an SSH private key; falls back to `~/.ssh`.
#[tauri::command]
pub(crate) async fn pick_ssh_key(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    current: Option<String>,
) -> Result<Option<String>, String> {
    let fallback = dirs::home_dir().map(|home| home.join(".ssh"));
    let start_dir = config::picker_start_dir(current.as_deref(), fallback.as_deref());
    pick_file_path(&app, &window, "Select SSH private key", start_dir).await
}

/// Run a native single-file picker while keeping the popover open, then
/// hand key focus back to it.
async fn pick_file_path(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    title: &str,
    start_dir: Option<std::path::PathBuf>,
) -> Result<Option<String>, String> {
    let mut dialog = app.dialog().file().set_title(title);
    if let Some(dir) = start_dir {
        dialog = dialog.set_directory(dir);
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let guard = NativeDialogGuard::open(app);
    dialog.pick_file(move |picked| {
        // The receiver only disappears if the command future was dropped,
        // in which case nobody is waiting for the answer.
        let _ = tx.send(picked);
    });
    let picked = rx
        .await
        .map_err(|_| "file picker closed without a result".to_string())?;
    drop(guard);

    if let Err(e) = window.set_focus() {
        tracing::warn!("failed to refocus window after file picker: {e}");
    }

    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|e| format!("selected file is not a local path: {e}"))?;
    Ok(Some(path.display().to_string()))
}

/// List the contexts in a kubeconfig (the default one when `path` is
/// `None`) so the form can offer them instead of free-typed names.
#[tauri::command]
pub(crate) async fn inspect_kubeconfig(
    path: Option<String>,
) -> Result<k8s::KubeconfigSummary, String> {
    k8s::inspect(path).await.map_err(|e| error::error_chain(&e))
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::validate_new_server;
    use crate::config::ServerConfig;

    #[tokio::test]
    async fn new_k8s_server_with_missing_kubeconfig_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("hippius1-oct.yaml").display().to_string();
        let server = ServerConfig::K8s {
            name: "hippius".to_string(),
            kubeconfig: Some(missing.clone()),
            context: "hippius".to_string(),
            namespace: "default".to_string(),
        };

        let err = validate_new_server(&server)
            .await
            .expect_err("missing kubeconfig must be rejected");

        assert!(err.contains(&missing), "{err}");
    }

    #[tokio::test]
    async fn new_ssh_server_is_not_validated_up_front() {
        let server = ServerConfig::Ssh {
            name: "bastion".to_string(),
            host: "10.0.1.50".to_string(),
            port: 22,
            user: "admin".to_string(),
            key_path: "/does/not/exist".to_string(),
        };

        validate_new_server(&server)
            .await
            .expect("ssh entries are checked by connecting, not on add");
    }
}
